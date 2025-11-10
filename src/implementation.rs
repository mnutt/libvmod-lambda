pub mod lambda_private {
    use tokio::runtime::Runtime;
    use tokio::sync::mpsc::{self, UnboundedSender};
    use tokio::sync::Semaphore;
    use tokio::time::Duration as TokioDuration;
    use aws_sdk_lambda::Client as LambdaClient;
    use aws_sdk_lambda::types::InvocationType;
    use bytes::Bytes;
    use std::error::Error;
    use std::sync::Arc;
    use std::sync::mpsc as std_mpsc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::sync::Mutex;
    use std::time::{Duration, Instant, SystemTime};
    use std::io::Write;

    use varnish::vcl::{Backend, Buffer, Ctx, Event, LogTag, Probe, VclBackend, VclError, VclResponse, VclResult, log};

    const MAX_CONCURRENT_INVOCATIONS: usize = 500_000;
    pub const DEFAULT_LAMBDA_TIMEOUT_SECS: u64 = 62;

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
        bitmap.wrapping_shl(64_u32 - window).count_ones()
    }

    fn is_healthy(bitmap: u64, window: u32, threshold: u32) -> bool {
        good_probes(bitmap, window) >= threshold
    }

    fn update_health(
        mut bitmap: u64,
        threshold: u32,
        window: u32,
        probe_ok: bool,
    ) -> (u64, bool, bool) {
        let old_health = is_healthy(bitmap, window, threshold);
        let new_bit = u64::from(probe_ok);
        bitmap = bitmap.wrapping_shl(1) | new_bit;
        let new_health = is_healthy(bitmap, window, threshold);
        (bitmap, new_health, new_health == old_health)
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
                let (bitmap, healthy, changed) =
                    update_health(bitmap, spec.threshold, spec.window, new_bit);
                log(
                    LogTag::BackendHealth,
                    format!(
                        "{} {} {} {} {} {} {} {} {} {}",
                        name,
                        if changed { "Went" } else { "Still" },
                        if healthy { "healthy" } else { "sick" },
                        "UNIMPLEMENTED",
                        good_probes(bitmap, spec.window),
                        spec.threshold,
                        spec.window,
                        time,
                        *avg.lock().unwrap(),
                        msg
                    ),
                );
                history.store(bitmap, Ordering::Relaxed);
                tokio::time::sleep(spec.interval).await;
            }
        }));
    }

    impl<'a> VclBackend<BackendResp> for VCLBackend {
        fn get_response(&self, ctx: &mut Ctx<'_>) -> VclResult<Option<BackendResp>> {
            if !self.healthy(ctx).0 {
                return Err("unhealthy".into());
            }

            let _bereq = ctx.http_bereq.as_ref().unwrap();

            // Get payload from request body
            let payload = unsafe {
                let bo = ctx.raw.bo.as_mut().unwrap();
                if !bo.bereq_body.is_null() {
                    // TODO: Read actual body from bereq_body
                    // For now, use empty payload
                    Vec::new()
                } else {
                    Vec::new()
                }
            };

            let req = InvokeRequest {
                function_name: self.function_name.clone(),
                payload,
                client: self.client.clone(),
                timeout_secs: self.timeout_secs,
            };

            let result = unsafe { (*self.bgt).invoke_sync(req) };

            match result {
                Err(e) => Err(e.into()),
                Ok(resp) => {
                    if let Some(err) = resp.function_error {
                        return Err(format!("Lambda error: {err}").into());
                    }

                    let beresp = ctx.http_beresp.as_mut().unwrap();
                    beresp.set_status(u16::try_from(resp.status_code).unwrap_or(500));
                    beresp.set_proto("HTTP/1.1")?;
                    beresp.set_header("Content-Type", "application/json")?;

                    Ok(Some(BackendResp {
                        payload: resp.payload,
                        cursor: 0,
                    }))
                }
            }
        }

        fn healthy(&self, _ctx: &mut Ctx<'_>) -> (bool, SystemTime) {
            let Some(ref probe_state) = self.probe_state else {
                return (true, SystemTime::UNIX_EPOCH);
            };

            assert!(probe_state.spec.window <= 64);

            let bitmap = probe_state.history.load(Ordering::Relaxed);
            (
                is_healthy(bitmap, probe_state.spec.window, probe_state.spec.threshold),
                probe_state.health_changed,
            )
        }

        fn event(&self, event: Event) {
            let Some(ref probe_state) = self.probe_state else {
                return;
            };

            let _guard = unsafe { (*self.bgt).rt.enter() };
            match event {
                Event::Warm => {
                    spawn_probe(
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
            if self.probe_state.is_none() {
                let state = if self.healthy(ctx).0 {
                    "healthy"
                } else {
                    "sick"
                };
                if json {
                    if detailed {
                        vsb.write(&"[0, 0, \"").unwrap();
                        vsb.write(&state).unwrap();
                        vsb.write(&"\"],").unwrap();
                    } else {
                        vsb.write(&"[]").unwrap();
                    }
                } else if detailed {
                    vsb.write(&"0/0\t").unwrap();
                    vsb.write(&state).unwrap();
                }
                return;
            }

            let ProbeState {
                history,
                avg,
                spec: Probe {
                    window, threshold, ..
                },
                ..
            } = self.probe_state.as_ref().unwrap();
            let bitmap = history.load(Ordering::Relaxed);
            let window = *window;
            let threshold = *threshold;
            let health_str = if is_healthy(bitmap, window, threshold) {
                "healthy"
            } else {
                "sick"
            };
            let msg = match (json, detailed) {
                (true, false) => {
                    format!(
                        "[{}, {}, \"{}\"]",
                        good_probes(bitmap, window),
                        window,
                        health_str,
                    )
                }
                (true, true) => {
                    serde_json::to_string(&self.probe_state.as_ref().unwrap().spec)
                        .as_ref()
                        .unwrap()
                        .to_owned()
                        + ",\n"
                }
                (false, false) => {
                    format!("{}/{}\t{}", good_probes(bitmap, window), window, health_str)
                }
                (false, true) => {
                    let mut s = format!(
                        "
 Current states  good: {:2} threshold: {:2} window: {:2}
  Average response time of good probes: {:.06}
  Oldest ================================================== Newest
  ",
                        good_probes(bitmap, window),
                        threshold,
                        window,
                        avg.lock().unwrap()
                    );
                    for i in 0..64 {
                        s += if bitmap.wrapping_shr(63 - i) & 1 == 1 {
                            "H"
                        } else {
                            "-"
                        };
                    }
                    s
                }
            };
            vsb.write(&msg).unwrap();
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
            let n = buf.write(to_write).unwrap();
            self.cursor += n;
            Ok(n)
        }

        fn len(&self) -> Option<usize> {
            self.payload.as_ref().map(|p| p.len())
        }
    }

    pub fn build_probe_state(
        mut probe: Probe,
        probe_payload: &str,
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

        Ok(ProbeState {
            spec: probe,
            history: AtomicU64::new(0),
            health_changed: SystemTime::now(),
            join_handle: None,
            payload: probe_payload.to_string(),
            avg: Mutex::new(0_f64),
        })
    }
}
