pub mod lambda_private {
    use tokio::runtime::Runtime;
    use tokio::sync::mpsc::{self, UnboundedSender};
    use tokio::sync::Semaphore;
    use tokio::time::Duration as TokioDuration;
    use aws_sdk_lambda::Client as LambdaClient;
    use aws_sdk_lambda::config::Builder as LambdaConfigBuilder;
    use aws_sdk_lambda::types::InvocationType;
    use aws_credential_types::Credentials;
    use aws_types::region::Region;
    use bytes::Bytes;
    use std::error::Error;
    use std::sync::Arc;
    use std::sync::mpsc as std_mpsc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant, SystemTime};
    use std::io::Write;
    use std::collections::HashMap;
    use serde::{Deserialize, Serialize};
    use url::Url;
    use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
    use std::os::raw::{c_uint, c_void};

    use varnish::vcl::{Backend, Buffer, Ctx, Event, LogTag, Probe, VclBackend, VclError, VclResponse, VclResult, log};
    use varnish::ffi::{BS_NONE, VRB_Iterate, ObjIterate};
    use varnish::{Vsc, VscMetric};

    /// Varnish statistics counters for Lambda backend
    #[derive(VscMetric)]
    #[repr(C)]
    pub struct LambdaBackendStats {
        /// Backend requests
        #[counter]
        pub req: AtomicU64,

        /// Request header bytes sent
        #[counter(format = "bytes")]
        pub bereq_hdrbytes: AtomicU64,

        /// Request body bytes sent
        #[counter(format = "bytes")]
        pub bereq_bodybytes: AtomicU64,

        /// Response header bytes received
        #[counter(format = "bytes")]
        pub beresp_hdrbytes: AtomicU64,

        /// Response body bytes received
        #[counter(format = "bytes")]
        pub beresp_bodybytes: AtomicU64,

        /// Failed requests
        #[counter]
        pub fail: AtomicU64,

        /// Lambda function errors (non-2xx status or FunctionError)
        #[counter]
        pub function_error: AtomicU64,

        /// Timeout errors
        #[counter]
        pub timeout: AtomicU64,

        /// Throttled requests (TooManyRequestsException)
        #[counter]
        pub throttled: AtomicU64,

        /// In-flight Lambda invocations
        #[gauge]
        pub inflight: AtomicU64,

        /// Cumulative Lambda invocation time in milliseconds
        #[counter(format = "duration")]
        pub invoke_time_ms: AtomicU64,

        /// Health probe bitmap (last 64 results)
        #[gauge]
        pub happy: AtomicU64,
    }

    /// Build a Lambda client with the specified configuration
    pub async fn build_lambda_client(region: Region, endpoint_url: Option<String>) -> LambdaClient {
        let sdk_config = if endpoint_url.is_some() {
            // Custom endpoint configuration: use credentials from environment if available
            let access_key = std::env::var("AWS_ACCESS_KEY_ID").unwrap_or_default();
            let secret_key = std::env::var("AWS_SECRET_ACCESS_KEY").unwrap_or_default();
            let session_token = std::env::var("AWS_SESSION_TOKEN").ok();

            aws_config::defaults(aws_config::BehaviorVersion::latest())
                .region(region.clone())
                .credentials_provider(Credentials::new(
                    access_key,
                    secret_key,
                    session_token,
                    None,
                    "custom-endpoint"
                ))
                .load()
                .await
        } else {
            // Production configuration: use real credentials from environment
            aws_config::from_env()
                .region(region.clone())
                .load()
                .await
        };

        let mut lambda_config = LambdaConfigBuilder::from(&sdk_config);
        if let Some(url) = endpoint_url {
            lambda_config = lambda_config.endpoint_url(url);
        }
        LambdaClient::from_conf(lambda_config.build())
    }

    /// Lambda HTTP request payload
    #[derive(Debug, Serialize, Deserialize)]
    struct LambdaHttpRequest {
        #[serde(rename = "httpMethod")]
        http_method: String,
        path: String,
        #[serde(rename = "queryStringParameters")]
        query_string_parameters: HashMap<String, String>,
        headers: HashMap<String, String>,
        body: String,
        #[serde(rename = "isBase64Encoded")]
        is_base64_encoded: bool,
    }

    /// Lambda HTTP response payload (JSON mode)
    #[derive(Debug, Serialize, Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct LambdaHttpResponse {
        #[serde(default)]
        is_base64_encoded: bool,
        status_code: u16,
        #[serde(default)]
        status_description: Option<String>,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        body: Option<String>,
    }

    /// Check if a content-type indicates text content
    fn is_text_content_type(content_type: &str) -> bool {
        let ct_lower = content_type.to_lowercase();
        ct_lower.starts_with("text/")
            || ct_lower.starts_with("application/json")
            || ct_lower.starts_with("application/javascript")
            || ct_lower.starts_with("application/xml")
            || ct_lower.starts_with("application/x-www-form-urlencoded")
    }

    /// Parse path into path and query string parameters
    fn parse_path(path_str: &str) -> (String, HashMap<String, String>) {
        // Varnish provides just the path, so we need a base URL for parsing
        let full_url = format!("http://dummy{}", path_str);

        match Url::parse(&full_url) {
            Ok(url) => {
                let path = url.path().to_string();
                let mut query_params = HashMap::new();

                for (key, value) in url.query_pairs() {
                    query_params.insert(key.to_string(), value.to_string());
                }

                (path, query_params)
            }
            Err(_) => {
                // Fallback to just the path if parsing fails
                (path_str.to_string(), HashMap::new())
            }
        }
    }

    /// Parse JSON response from Lambda
    fn parse_json_response(payload: &[u8]) -> VclResult<(u16, HashMap<String, String>, Vec<u8>)> {
        let response: LambdaHttpResponse = serde_json::from_slice(payload)
            .map_err(|e| format!("Failed to parse Lambda JSON response: {}", e))?;

        let body_bytes = if let Some(body) = response.body {
            if response.is_base64_encoded {
                BASE64.decode(body.as_bytes())
                    .map_err(|e| format!("Failed to decode base64 body: {}", e))?
            } else {
                body.into_bytes()
            }
        } else {
            Vec::new()
        };

        Ok((response.status_code, response.headers, body_bytes))
    }

    /// Parse raw HTTP response from Lambda
    ///
    /// The expected format of the payload (using \r\n as line endings):
    /// ```
    /// HTTP/1.1 200 OK
    /// Content-Type: application/json
    /// Content-Length: 12
    ///
    /// {"message":"Hello, world!"}
    /// ```
    fn parse_raw_http_response(payload: &[u8]) -> VclResult<(u16, HashMap<String, String>, Vec<u8>)> {
        let response_str = std::str::from_utf8(payload)
            .map_err(|e| format!("Invalid UTF-8 in raw HTTP response: {}", e))?;

        let mut lines = response_str.lines();

        // Parse status line: "HTTP/1.1 200 OK"
        let status_line = lines.next()
            .ok_or("Empty HTTP response")?;

        let status_code = status_line
            .split_whitespace()
            .nth(1)
            .and_then(|s| s.parse::<u16>().ok())
            .ok_or_else(|| format!("Invalid HTTP status line: {}", status_line))?;

        // Parse headers
        let mut headers = HashMap::new();
        let mut body_start = 0;

        for (idx, line) in lines.enumerate() {
            if line.is_empty() {
                // Empty line marks end of headers
                // Calculate byte offset for body start
                // Note: lines() strips \r\n, but actual data has \r\n (2 bytes)
                body_start = status_line.len() + 2; // +2 for \r\n
                for h_line in response_str.lines().take(idx + 1).skip(1) {
                    body_start += h_line.len() + 2; // +2 for \r\n
                }
                body_start += 2; // Final empty line \r\n
                break;
            }

            if let Some((name, value)) = line.split_once(':') {
                headers.insert(
                    name.trim().to_lowercase(),
                    value.trim().to_string()
                );
            }
        }

        // Extract body
        let body_bytes = if body_start < payload.len() {
            payload[body_start..].to_vec()
        } else {
            Vec::new()
        };

        Ok((status_code, headers, body_bytes))
    }

    const MAX_CONCURRENT_INVOCATIONS: usize = 500_000;
    pub const DEFAULT_LAMBDA_TIMEOUT_SECS: u64 = 62;

    /// Extract and encode the request body from Varnish context
    ///
    /// Returns a tuple of (body_string, is_base64_encoded):
    /// - For text content types: returns the body as UTF-8 string, not base64 encoded
    /// - For binary content types: returns base64-encoded body
    /// - If no body is available: returns empty string, not base64 encoded
    fn extract_request_body(ctx: &mut Ctx, content_type: &str) -> (String, bool) {
        // Callback function to collect body chunks from Varnish
        unsafe extern "C" fn body_collect_iterate(
            priv_: *mut c_void,
            _flush: c_uint,
            ptr: *const c_void,
            l: isize,
        ) -> i32 {
            // Nothing to do if no data
            if ptr.is_null() || l == 0 {
                return 0;
            }
            unsafe {
                let body_vec = priv_.cast::<Vec<u8>>().as_mut().unwrap();
                let buf = std::slice::from_raw_parts(ptr.cast::<u8>(), l as usize);
                body_vec.extend_from_slice(buf);
            }
            0
        }

        let mut body_bytes = Vec::new();

        let result = unsafe {
            let bo = ctx.raw.bo.as_mut().unwrap();
            let p = (&raw mut body_bytes).cast::<c_void>();

            // Try to iterate over the request body
            // ObjIterate is used when bereq_body is available, otherwise VRB_Iterate
            if bo.bereq_body.is_null() {
                if !bo.req.is_null() && (*bo.req).req_body_status != BS_NONE.as_ptr() {
                    VRB_Iterate(
                        bo.wrk,
                        bo.vsl.as_mut_ptr(),
                        bo.req,
                        Some(body_collect_iterate),
                        p,
                    )
                } else {
                    -1  // No body available
                }
            } else {
                ObjIterate(bo.wrk, bo.bereq_body, p, Some(body_collect_iterate), 0) as isize
            }
        };

        if result < 0 || body_bytes.is_empty() {
            (String::new(), false)
        } else if is_text_content_type(content_type) {
            // Text content - use as-is
            (String::from_utf8_lossy(&body_bytes).to_string(), false)
        } else {
            // Binary content - base64 encode
            (BASE64.encode(&body_bytes), true)
        }
    }

    /// Background runtime for async Lambda invocations
    pub struct BgThread {
        pub rt: Runtime,
        pub sender: UnboundedSender<(InvokeRequest, std_mpsc::Sender<Result<InvokeResponse, String>>)>,
    }

    /// Request to invoke a Lambda function
    pub struct InvokeRequest {
        pub function_name: String,
        pub payload: Vec<u8>,
        pub client: LambdaClient,
        pub timeout_secs: u64,
    }

    /// Response from Lambda invocation
    pub struct InvokeResponse {
        pub status_code: i32,
        pub payload: Option<Bytes>,
        pub function_error: Option<String>,
    }

    /// Probe state for health checking
    pub struct ProbeState {
        pub spec: Probe,
        pub history: AtomicU64,
        pub health_changed: SystemTime,
        pub payload: String,
        pub join_handle: Option<tokio::task::JoinHandle<()>>,
        pub avg: Mutex<f64>,
    }

    /// Lambda backend object
    #[allow(non_camel_case_types)]
    pub struct backend {
        #[allow(dead_code)]
        pub name: String,
        pub be: Backend<VCLBackend, BackendResp>,
    }

    /// VCL Backend implementation for Lambda
    pub struct VCLBackend {
        #[allow(dead_code)]
        pub name: String,
        pub function_name: String,
        pub bgt: *const BgThread,
        pub client: LambdaClient,
        pub timeout_secs: u64,
        pub probe_state: Option<ProbeState>,
        pub raw_response_mode: bool,
        pub stats: Vsc<LambdaBackendStats>,
    }

    impl VCLBackend {
        /// Get a reference to the background thread
        ///
        /// # Safety
        /// This is safe because:
        /// - The BgThread is created during VCL load and lives until VCL discard
        /// - The VCLBackend is only accessed during request processing, which happens
        ///   within the VCL lifetime
        /// - The pointer is guaranteed to remain valid for the lifetime of the VCL
        fn bgt(&self) -> &BgThread {
            unsafe { &*self.bgt }
        }
    }

    /// Backend response implementation
    pub struct BackendResp {
        pub payload: Option<Bytes>,
        pub cursor: usize,
    }

    impl BgThread {
        pub fn new() -> Result<Self, Box<dyn Error>> {
            let rt = Runtime::new()?;
            let (sender, mut receiver) = mpsc::unbounded_channel::<(InvokeRequest, std_mpsc::Sender<Result<InvokeResponse, String>>)>();
            let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_INVOCATIONS));

            rt.spawn(async move {
                while let Some((req, resp_tx)) = receiver.recv().await {
                    let sem = semaphore.clone();

                    tokio::spawn(async move {
                        let _permit = sem.acquire().await.expect("semaphore closed");

                        let result = invoke_lambda(&req.client, &req.function_name, req.payload, req.timeout_secs).await;
                        let _ = resp_tx.send(result);
                    });
                }
            });

            Ok(BgThread { rt, sender })
        }

        pub fn invoke_sync(&self, req: InvokeRequest) -> Result<InvokeResponse, String> {
            let (tx, rx) = std_mpsc::channel();
            self.sender.send((req, tx)).map_err(|e| e.to_string())?;
            rx.recv().map_err(|e| e.to_string())?
        }
    }

    pub async fn invoke_lambda(
        client: &LambdaClient,
        function_name: &str,
        payload: Vec<u8>,
        timeout_secs: u64,
    ) -> Result<InvokeResponse, String> {
        let invoke_future = client
            .invoke()
            .function_name(function_name)
            .invocation_type(InvocationType::RequestResponse)
            .payload(aws_sdk_lambda::primitives::Blob::new(payload))
            .send();

        let result = tokio::time::timeout(
            TokioDuration::from_secs(timeout_secs),
            invoke_future
        )
        .await
        .map_err(|_| format!("Lambda invocation timed out after {}s", timeout_secs))?
        .map_err(|e| format!("Lambda invocation failed: {:?}", e))?;

        Ok(InvokeResponse {
            status_code: result.status_code(),
            payload: result.payload().map(|b| Bytes::copy_from_slice(b.as_ref())),
            function_error: result.function_error().map(String::from),
        })
    }

    fn good_probes(bitmap: u64, window: u32) -> u32 {
        // Mask the rightmost 'window' bits and count the 1s
        let mask = if window >= 64 {
            u64::MAX
        } else {
            (1u64 << window) - 1
        };
        (bitmap & mask).count_ones()
    }

    fn is_healthy(bitmap: u64, window: u32, threshold: u32) -> bool {
        good_probes(bitmap, window) >= threshold
    }

    /// Result of updating health probe history
    struct HealthUpdate {
        /// Updated bitmap with new probe result
        bitmap: u64,
        /// Current health status (true = healthy)
        is_healthy: bool,
        /// Whether health status remained unchanged
        health_unchanged: bool,
    }

    fn update_health(
        mut bitmap: u64,
        threshold: u32,
        window: u32,
        probe_ok: bool,
    ) -> HealthUpdate {
        let old_health = is_healthy(bitmap, window, threshold);
        let new_bit = u64::from(probe_ok);
        bitmap = bitmap.wrapping_shl(1) | new_bit;
        let new_health = is_healthy(bitmap, window, threshold);
        HealthUpdate {
            bitmap,
            is_healthy: new_health,
            health_unchanged: new_health == old_health,
        }
    }

    fn spawn_probe(bgt: &'static BgThread, probe_state: *mut ProbeState, name: String, client: LambdaClient) {
        let probe_state = unsafe { probe_state.as_mut().unwrap() };
        let spec = probe_state.spec.clone();
        let payload = probe_state.payload.clone();
        let history = &probe_state.history;
        let avg = &probe_state.avg;

        probe_state.join_handle = Some(bgt.rt.spawn(async move {
            let mut h = 0_u64;
            for i in 0..std::cmp::min(spec.initial, 64) {
                h |= 1 << i;
            }
            history.store(h, Ordering::Relaxed);
            let mut avg_rate = 0_f64;

            loop {
                let msg;
                let mut time = 0_f64;

                let start = Instant::now();
                let result = invoke_lambda(
                    &client,
                    &name,
                    payload.as_bytes().to_vec(),
                    spec.timeout.as_secs(),
                ).await;

                let new_bit = match result {
                    Err(e) => {
                        msg = format!("Error: {e}");
                        false
                    }
                    Ok(resp) if resp.function_error.is_none() && resp.status_code == 200 => {
                        msg = format!("Success: status {}", resp.status_code);
                        if avg_rate < 4.0 {
                            avg_rate += 1.0;
                        }
                        time = start.elapsed().as_secs_f64();
                        let mut avg = avg.lock().unwrap();
                        *avg += (time - *avg) / avg_rate;
                        true
                    }
                    Ok(resp) => {
                        msg = format!(
                            "Error: status {}, function_error: {:?}",
                            resp.status_code,
                            resp.function_error
                        );
                        false
                    }
                };

                let bitmap = history.load(Ordering::Relaxed);
                let health_update = update_health(bitmap, spec.threshold, spec.window, new_bit);
                log(
                    LogTag::BackendHealth,
                    format!(
                        "{} {} {} {} {} {} {} {} {} {}",
                        name,
                        if health_update.health_unchanged { "Still" } else { "Went" },
                        if health_update.is_healthy { "healthy" } else { "sick" },
                        name,
                        good_probes(health_update.bitmap, spec.window),
                        spec.threshold,
                        spec.window,
                        time,
                        *avg.lock().unwrap(),
                        msg
                    ),
                );
                history.store(health_update.bitmap, Ordering::Relaxed);
                tokio::time::sleep(spec.interval).await;
            }
        }));
    }

    impl VclBackend<BackendResp> for VCLBackend {
        fn get_response(&self, ctx: &mut Ctx<'_>) -> VclResult<Option<BackendResp>> {
            // Increment request counter
            self.stats.req.fetch_add(1, Ordering::Relaxed);

            if !self.healthy(ctx).0 {
                self.stats.fail.fetch_add(1, Ordering::Relaxed);
                return Err("unhealthy".into());
            }

            let bereq = ctx.http_bereq.as_ref().unwrap();

            // Extract HTTP method
            let http_method = bereq.method()
                .map(|m| String::from_utf8_lossy(m.as_ref()).into_owned())
                .unwrap_or_else(|| "GET".into());

            // Extract URL and parse path/query
            let url = bereq.url()
                .map(|m| String::from_utf8_lossy(m.as_ref()).into_owned())
                .unwrap_or_else(|| "/".into());
            let (path, query_string_parameters) = parse_path(&url);

            // Extract headers
            let mut headers = HashMap::new();
            let mut header_bytes = 0u64;
            for (name, value) in bereq {
                let value_str = String::from_utf8_lossy(value.as_ref()).into_owned();
                // Count header bytes: "name: value\r\n"
                header_bytes += name.len() as u64 + 2 + value_str.len() as u64 + 2;
                headers.insert(name.to_lowercase(), value_str);
            }

            // Determine content type
            let content_type = headers.get("content-type")
                .map(|s| s.as_str())
                .unwrap_or("");

            // Get and encode request body
            let (body, is_base64_encoded) = extract_request_body(ctx, content_type);
            let body_bytes = body.len() as u64;

            // Build Lambda HTTP request payload
            let lambda_request = LambdaHttpRequest {
                http_method,
                path,
                query_string_parameters,
                headers,
                body,
                is_base64_encoded,
            };

            // Serialize to JSON
            let payload = serde_json::to_vec(&lambda_request)
                .map_err(|e| {
                    self.stats.fail.fetch_add(1, Ordering::Relaxed);
                    format!("Failed to serialize request: {}", e)
                })?;

            // Update request byte counters
            self.stats.bereq_hdrbytes.fetch_add(header_bytes, Ordering::Relaxed);
            self.stats.bereq_bodybytes.fetch_add(body_bytes, Ordering::Relaxed);

            let req = InvokeRequest {
                function_name: self.function_name.clone(),
                payload,
                client: self.client.clone(),
                timeout_secs: self.timeout_secs,
            };

            // Track in-flight invocations and measure latency
            self.stats.inflight.fetch_add(1, Ordering::Relaxed);
            let start = Instant::now();
            let result = self.bgt().invoke_sync(req);
            let elapsed_ms = start.elapsed().as_millis() as u64;
            self.stats.inflight.fetch_sub(1, Ordering::Relaxed);
            self.stats.invoke_time_ms.fetch_add(elapsed_ms, Ordering::Relaxed);

            match result {
                Err(e) => {
                    // Check error type
                    let err_str = e.to_string();
                    if err_str.contains("timeout") || err_str.contains("timed out") {
                        self.stats.timeout.fetch_add(1, Ordering::Relaxed);
                    } else if err_str.contains("TooManyRequestsException") || err_str.contains("ThrottlingException") {
                        self.stats.throttled.fetch_add(1, Ordering::Relaxed);
                    }
                    self.stats.fail.fetch_add(1, Ordering::Relaxed);
                    Err(e.into())
                }
                Ok(resp) => {
                    if let Some(err) = resp.function_error {
                        self.stats.function_error.fetch_add(1, Ordering::Relaxed);
                        self.stats.fail.fetch_add(1, Ordering::Relaxed);
                        return Err(format!("Lambda error: {err}").into());
                    }

                    let Some(payload) = resp.payload else {
                        self.stats.fail.fetch_add(1, Ordering::Relaxed);
                        return Err("Empty Lambda response".into());
                    };

                    // Parse response based on mode
                    let (status_code, headers, body_bytes) = if self.raw_response_mode {
                        parse_raw_http_response(&payload)?
                    } else {
                        parse_json_response(&payload)?
                    };

                    // Track function errors based on status code
                    if !(200..300).contains(&status_code) {
                        self.stats.function_error.fetch_add(1, Ordering::Relaxed);
                    }

                    // Calculate response header bytes
                    let mut resp_header_bytes = 0u64;
                    for (name, value) in &headers {
                        resp_header_bytes += name.len() as u64 + 2 + value.len() as u64 + 2;
                    }
                    self.stats.beresp_hdrbytes.fetch_add(resp_header_bytes, Ordering::Relaxed);
                    self.stats.beresp_bodybytes.fetch_add(body_bytes.len() as u64, Ordering::Relaxed);

                    // Set response status and headers
                    let beresp = ctx.http_beresp.as_mut().unwrap();
                    beresp.set_status(status_code);
                    beresp.set_proto("HTTP/1.1")?;

                    // Set all headers from the response
                    for (name, value) in headers {
                        beresp.set_header(&name, &value)?;
                    }

                    Ok(Some(BackendResp {
                        payload: Some(Bytes::from(body_bytes)),
                        cursor: 0,
                    }))
                }
            }
        }

        fn healthy(&self, _ctx: &mut Ctx<'_>) -> (bool, SystemTime) {
            let Some(ref probe_state) = self.probe_state else {
                // No probe: always healthy, update gauge
                self.stats.happy.store(u64::MAX, Ordering::Relaxed);
                return (true, SystemTime::UNIX_EPOCH);
            };

            assert!(probe_state.spec.window <= 64);

            let bitmap = probe_state.history.load(Ordering::Relaxed);
            // Update the happy gauge with the current probe bitmap
            self.stats.happy.store(bitmap, Ordering::Relaxed);

            (
                is_healthy(bitmap, probe_state.spec.window, probe_state.spec.threshold),
                probe_state.health_changed,
            )
        }

        fn event(&self, event: Event) {
            let Some(ref probe_state) = self.probe_state else {
                return;
            };

            let _guard = self.bgt().rt.enter();
            match event {
                Event::Warm => {
                    spawn_probe(
                        // SAFETY: BgThread lives for the entire VCL lifetime, and spawn_probe
                        // needs a 'static reference to spawn a background task
                        unsafe { &*self.bgt },
                        std::ptr::from_ref::<ProbeState>(probe_state).cast_mut(),
                        self.function_name.clone(),
                        self.client.clone(),
                    );
                }
                Event::Cold => {
                    probe_state.join_handle.as_ref().unwrap().abort();
                }
                _ => {}
            }
        }

        fn list(&self, ctx: &mut Ctx<'_>, vsb: &mut Buffer<'_>, detailed: bool, json: bool) {
            // Handle backends without probe configuration
            let Some(probe_state) = self.probe_state.as_ref() else {
                let state = if self.healthy(ctx).0 { "healthy" } else { "sick" };
                let msg = match (json, detailed) {
                    (true, true) => format!("[0, 0, \"{}\"],", state),
                    (true, false) => "[]".to_string(),
                    (false, true) => format!("0/0\t{}", state),
                    (false, false) => String::new(),
                };
                let _ = vsb.write(&msg);
                return;
            };

            // Extract probe state information
            let ProbeState {
                history,
                avg,
                spec: Probe { window, threshold, .. },
                ..
            } = probe_state;
            let bitmap = history.load(Ordering::Relaxed);
            let window = *window;
            let threshold = *threshold;
            let good_count = good_probes(bitmap, window);
            let health_str = if is_healthy(bitmap, window, threshold) { "healthy" } else { "sick" };

            // Format output based on json/detailed flags
            let msg = match (json, detailed) {
                (true, false) => {
                    format!("[{}, {}, \"{}\"]", good_count, window, health_str)
                }
                (true, true) => {
                    format!("{},\n", serde_json::to_string(&probe_state.spec).unwrap())
                }
                (false, false) => {
                    format!("{}/{}\t{}", good_count, window, health_str)
                }
                (false, true) => {
                    let bitmap_viz: String = (0..64)
                        .map(|i| if bitmap.wrapping_shr(63 - i) & 1 == 1 { "H" } else { "-" })
                        .collect();
                    format!(
                        "
 Current states  good: {:2} threshold: {:2} window: {:2}
  Average response time of good probes: {:.06}
  Oldest ================================================== Newest
  {}",
                        good_count,
                        threshold,
                        window,
                        *avg.lock().unwrap(),
                        bitmap_viz
                    )
                }
            };
            let _ = vsb.write(&msg);
        }
    }

    impl VclResponse for BackendResp {
        fn read(&mut self, mut buf: &mut [u8]) -> VclResult<usize> {
            let Some(ref payload) = self.payload else {
                return Ok(0);
            };

            if self.cursor >= payload.len() {
                return Ok(0);
            }

            let to_write = &payload[self.cursor..];
            let n = buf.write(to_write)
                .map_err(|e| format!("Failed to write response body: {}", e))?;
            self.cursor += n;
            Ok(n)
        }

        fn len(&self) -> Option<usize> {
            self.payload.as_ref().map(|p| p.len())
        }
    }

    pub fn build_probe_state(
        mut probe: Probe,
    ) -> Result<ProbeState, VclError> {
        // Sanitize probe (see vbp_set_defaults in Varnish Cache)
        if probe.timeout.is_zero() {
            probe.timeout = Duration::from_secs(2);
        }
        if probe.interval.is_zero() {
            probe.interval = Duration::from_secs(5);
        }
        if probe.window == 0 {
            probe.window = 8;
        }
        if probe.threshold == 0 {
            probe.threshold = 3;
        }
        if probe.initial == 0 {
            probe.initial = probe.threshold - 1;
        }
        probe.initial = std::cmp::min(probe.initial, probe.threshold);

        // Construct Lambda HTTP request from probe definition
        use varnish::vcl::Request;
        let payload = match &probe.request {
            Request::Url(url) => {
                // Construct a GET request to the specified URL path
                let lambda_request = LambdaHttpRequest {
                    http_method: "GET".to_string(),
                    path: url.to_string(),
                    query_string_parameters: HashMap::new(),
                    headers: HashMap::new(),
                    body: String::new(),
                    is_base64_encoded: false,
                };
                serde_json::to_string(&lambda_request)
                    .map_err(|e| VclError::new(format!("Failed to serialize probe request: {}", e)))?
            }
            Request::Text(_) => {
                // TODO: Parse full HTTP request text into LambdaHttpRequest
                // For now, text-based probes are not supported
                return Err(VclError::new(
                    "Text-based probe requests (.request) are not yet supported. Use .url instead".to_string()
                ));
            }
        };

        Ok(ProbeState {
            spec: probe,
            history: AtomicU64::new(0),
            health_changed: SystemTime::now(),
            join_handle: None,
            payload,
            avg: Mutex::new(0_f64),
        })
    }
}
