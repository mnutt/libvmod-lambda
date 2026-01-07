mod aws_client;
mod aws_client_pool;
mod implementation;

use varnish::run_vtc_tests;
run_vtc_tests!("tests/*.vtc");

#[varnish::vmod(docs = "API.md")]
mod lambda {
    use std::error::Error;
    use std::sync::atomic::Ordering;
    use std::time::Duration;

    use aws_types::region::Region;
    use varnish::ffi::VCL_BACKEND;
    use varnish::vcl::{Backend, Ctx, Event, Probe, VclError};

    use crate::implementation::lambda_private::{
        build_probe_state, backend, BgThread, InvokeRequest, VCLBackend,
        DEFAULT_LAMBDA_TIMEOUT_SECS, ResponseFormat,
    };
    use crate::aws_client_pool::{ClientPool, DEFAULT_POOL_SIZE};

    impl backend {
        #[allow(clippy::too_many_arguments)]
        /// Create a new Lambda backend
        pub fn new(
            ctx: &mut Ctx,
            #[vcl_name] vcl_name: &str,
            #[shared_per_vcl] vp_vcl: &mut Option<Box<BgThread>>,
            /// Lambda function name or ARN
            function_name: &str,
            /// AWS region (e.g., "us-east-1")
            region: &str,
            /// Optional custom endpoint URL (e.g., for LocalStack)
            endpoint_url: Option<&str>,
            /// Lambda invocation timeout in seconds (default: 62)
            timeout: Option<Duration>,
            /// Health probe configuration
            probe: Option<Probe>,
            /// Response format: "json" (default) or "http" (C++ runtime only)
            #[default("json")]
            response_format: &str,
            /// Number of Lambda clients in the pool (default: 64)
            /// Each client maintains its own HTTP/2 connection for parallel requests
            pool_size: Option<i64>,
        ) -> Result<Self, VclError> {
            // Use the BgThread's runtime to initialize the client pool
            // This ensures clients are created in the same runtime context they will be used in
            let bg = vp_vcl.as_ref().expect("BgThread not initialized");
            let region_obj = Region::new(region.to_string());
            let endpoint_url_opt = endpoint_url.map(String::from);
            let pool_size = match pool_size {
                Some(s) if s > 0 => s as usize,
                Some(s) => return Err(VclError::new(format!(
                    "Invalid pool_size '{}': must be a positive integer",
                    s
                ))),
                None => DEFAULT_POOL_SIZE,
            };
            let pool = bg.rt.block_on(ClientPool::build(region_obj, endpoint_url_opt, pool_size));

            let timeout_secs = timeout
                .map(|d| d.as_secs())
                .unwrap_or(DEFAULT_LAMBDA_TIMEOUT_SECS);

            // Parse response format
            let response_format = match response_format {
                "json" => ResponseFormat::Json,
                "http" => ResponseFormat::Http,
                _ => return Err(VclError::new(format!(
                    "Invalid response_format '{}': must be 'json' or 'http'",
                    response_format
                ))),
            };

            let has_probe = probe.is_some();

            // Create VSC statistics for this backend
            // Format: lambda.{vcl_name}.{metric_name}
            let stats = varnish::Vsc::new(
                "lambda",
                vcl_name,
            );

            let probe_state = match probe {
                Some(spec) => Some(build_probe_state(spec, vcl_name).map_err(|e| {
                    VclError::new(format!("lambda: failed to add probe to {vcl_name} ({e})"))
                })?),
                None => None,
            };

            let be = Backend::new(
                ctx,
                "lambda",
                vcl_name,
                VCLBackend {
                    name: vcl_name.to_string(),
                    function_name: function_name.to_string(),
                    bgt: &raw const **vp_vcl.as_ref().unwrap(),
                    pool,
                    timeout_secs,
                    probe_state,
                    response_format,
                    stats,
                },
                has_probe,
            )?;

            Ok(backend {
                name: vcl_name.to_owned(),
                be,
            })
        }

        /// Invoke the Lambda function with a JSON payload and return the response
        /// This is a synchronous call from VCL's perspective
        pub fn invoke(
            &self,
            #[shared_per_vcl] vp_vcl: Option<&BgThread>,
            /// JSON payload to send to Lambda
            payload: &str,
        ) -> Result<String, Box<dyn Error>> {
            let bg = vp_vcl.ok_or("VMOD not initialized")?;
            let vcl_backend = self.be.get_inner();

            let client_guard = vcl_backend.pool.acquire();
            let req = InvokeRequest {
                function_name: vcl_backend.function_name.clone(),
                payload: payload.as_bytes().to_vec(),
                client: client_guard.client().clone(),
                timeout_secs: vcl_backend.timeout_secs,
            };

            vcl_backend.stats.inflight.fetch_add(1, Ordering::Relaxed);
            let result = bg.invoke_sync(req);
            vcl_backend.stats.inflight.fetch_sub(1, Ordering::Relaxed);
            let result = result?;

            // Return the payload as a string, or error message
            if let Some(error) = result.function_error {
                Err(format!("Lambda error: {}", error).into())
            } else if let Some(payload) = result.payload {
                String::from_utf8(payload.to_vec())
                    .map_err(|e| format!("Invalid UTF-8 in response: {}", e).into())
            } else {
                Ok(String::new())
            }
        }

        /// Get the Lambda function name
        pub fn get_function_name(&self, _ctx: &Ctx) -> String {
            self.be.get_inner().function_name.clone()
        }

        /// Get the AWS region
        pub fn get_region(&self, _ctx: &Ctx) -> String {
            self.be
                .get_inner()
                .pool
                .first_client()
                .config()
                .region()
                .map(|r| r.to_string())
                .unwrap_or_else(|| "unknown".into())
        }

        /// Return a VCL backend built upon the Lambda backend specification
        pub unsafe fn backend(&self) -> VCL_BACKEND {
            self.be.vcl_ptr()
        }
    }

    #[event]
    pub fn event(
        #[shared_per_vcl] vp_vcl: &mut Option<Box<BgThread>>,
        event: Event,
    ) {
        if let Event::Load = event {
            *vp_vcl = Some(Box::new(BgThread::new().unwrap()));
        }
    }
}
