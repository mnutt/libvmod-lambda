use tokio::runtime::Runtime;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::oneshot;
use tokio::sync::Semaphore;
use tokio::time::Duration;
use aws_sdk_lambda::Client as LambdaClient;
use aws_sdk_lambda::types::InvocationType;
use aws_types::region::Region;
use bytes::Bytes;
use std::error::Error;
use std::sync::Arc;

const MAX_CONCURRENT_INVOCATIONS: usize = 500_000;
const DEFAULT_LAMBDA_TIMEOUT_SECS: u64 = 62;

/// Background runtime for async Lambda invocations
pub struct BgThread {
    #[allow(dead_code)]
    rt: Runtime,
    sender: UnboundedSender<(InvokeRequest, oneshot::Sender<Result<InvokeResponse, String>>)>,
}

/// Request to invoke a Lambda function
struct InvokeRequest {
    function_name: String,
    payload: Vec<u8>,
    client: LambdaClient,
}

/// Response from Lambda invocation
struct InvokeResponse {
    #[allow(dead_code)]
    status_code: i32,
    payload: Option<Bytes>,
    function_error: Option<String>,
}

/// Lambda backend object
pub struct Backend {
    function_name: String,
    region: String,
    endpoint_url: Option<String>,
    client: LambdaClient,
    #[allow(dead_code)]
    runtime: Runtime,
}

impl BgThread {
    fn new() -> Result<Self, Box<dyn Error>> {
        let rt = Runtime::new()?;
        let (sender, mut receiver) = mpsc::unbounded_channel::<(InvokeRequest, oneshot::Sender<Result<InvokeResponse, String>>)>();
        let semaphore = Arc::new(Semaphore::new(MAX_CONCURRENT_INVOCATIONS));

        rt.spawn(async move {
            while let Some((req, resp_tx)) = receiver.recv().await {
                let sem = semaphore.clone();

                tokio::spawn(async move {
                    let _permit = sem.acquire().await.expect("semaphore closed");

                    let result = invoke_lambda(&req.client, &req.function_name, req.payload).await;
                    let _ = resp_tx.send(result);
                });
            }
        });

        Ok(BgThread { rt, sender })
    }
}

async fn invoke_lambda(
    client: &LambdaClient,
    function_name: &str,
    payload: Vec<u8>,
) -> Result<InvokeResponse, String> {
    let invoke_future = client
        .invoke()
        .function_name(function_name)
        .invocation_type(InvocationType::RequestResponse)
        .payload(aws_sdk_lambda::primitives::Blob::new(payload))
        .send();

    let result = tokio::time::timeout(
        Duration::from_secs(DEFAULT_LAMBDA_TIMEOUT_SECS),
        invoke_future
    )
    .await
    .map_err(|_| format!("Lambda invocation timed out after {}s", DEFAULT_LAMBDA_TIMEOUT_SECS))?
    .map_err(|e| format!("Lambda invocation failed: {}", e))?;

    Ok(InvokeResponse {
        status_code: result.status_code(),
        payload: result.payload().map(|b| Bytes::copy_from_slice(b.as_ref())),
        function_error: result.function_error().map(String::from),
    })
}

#[varnish::vmod]
mod lambda {
    use super::*;
    use varnish::vcl::{Ctx, Event};

    /// Event handler for VCL lifecycle
    #[event]
    pub fn event(
        #[shared_per_vcl] vp_vcl: &mut Option<Box<BgThread>>,
        event: Event,
    ) {
        if let Event::Load = event {
            *vp_vcl = Some(Box::new(BgThread::new().unwrap()));
        }
    }

    impl Backend {
        /// Create a new Lambda backend
        pub fn new(
            _ctx: &Ctx,
            #[vcl_name] _vcl_name: &str,
            /// Lambda function name or ARN
            function_name: &str,
            /// AWS region (e.g., "us-east-1")
            region: &str,
            /// Optional custom endpoint URL (e.g., for LocalStack). Pass empty string to use default.
            endpoint_url: &str,
        ) -> Self {
            // Create the Lambda client with the specified region and optional endpoint
            let endpoint_url_opt = if endpoint_url.is_empty() {
                None
            } else {
                Some(endpoint_url.to_string())
            };

            // Create a runtime for this backend - it will persist for the backend's lifetime
            // The AWS SDK client needs the runtime to stay alive for HTTP operations
            let runtime = Runtime::new().expect("Failed to create runtime");
            let region_obj = Region::new(region.to_string());
            let endpoint_for_client = endpoint_url_opt.clone();
            let client = runtime.block_on(async move {
                let mut config = aws_config::from_env().region(region_obj);
                if let Some(url) = endpoint_for_client {
                    config = config.endpoint_url(url);
                }
                let config = config.load().await;
                LambdaClient::new(&config)
            });

            Backend {
                function_name: function_name.to_string(),
                region: region.to_string(),
                endpoint_url: endpoint_url_opt,
                client,
                runtime,
            }
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

            let req = InvokeRequest {
                function_name: self.function_name.clone(),
                payload: payload.as_bytes().to_vec(),
                client: self.client.clone(),
            };

            let (tx, rx) = oneshot::channel();
            bg.sender.send((req, tx))?;

            // Block until Lambda responds
            // This uses tokio's block_in_place to allow other tasks to progress
            let result = tokio::task::block_in_place(|| {
                tokio::runtime::Handle::current().block_on(rx)
            })??;

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
            self.function_name.clone()
        }

        /// Get the AWS region
        pub fn get_region(&self, _ctx: &Ctx) -> String {
            self.region.clone()
        }
    }
}

#[cfg(test)]
varnish::run_vtc_tests!("tests/*.vtc");
