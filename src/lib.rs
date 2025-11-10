use tokio::runtime::Runtime;
use tokio::sync::mpsc::{self, UnboundedSender};
use tokio::sync::Semaphore;
use tokio::time::Duration;
use aws_sdk_lambda::Client as LambdaClient;
use aws_sdk_lambda::config::Builder as LambdaConfigBuilder;
use aws_sdk_lambda::types::InvocationType;
use aws_types::region::Region;
use aws_credential_types::Credentials;
use bytes::Bytes;
use std::error::Error;
use std::sync::Arc;
use std::sync::mpsc as std_mpsc;

const MAX_CONCURRENT_INVOCATIONS: usize = 500_000;
const DEFAULT_LAMBDA_TIMEOUT_SECS: u64 = 62;

/// Background runtime for async Lambda invocations
pub struct BgThread {
    #[allow(dead_code)]
    rt: Runtime,
    sender: UnboundedSender<(InvokeRequest, std_mpsc::Sender<Result<InvokeResponse, String>>)>,
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
#[allow(non_camel_case_types)]
pub struct backend {
    function_name: String,
    region: String,
    client: LambdaClient,
}

impl BgThread {
    fn new() -> Result<Self, Box<dyn Error>> {
        let rt = Runtime::new()?;
        let (sender, mut receiver) = mpsc::unbounded_channel::<(InvokeRequest, std_mpsc::Sender<Result<InvokeResponse, String>>)>();
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
    .map_err(|e| format!("Lambda invocation failed: {:?}", e))?;

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

    impl backend {
        /// Create a new Lambda backend
        pub fn new(
            _ctx: &Ctx,
            #[vcl_name] _vcl_name: &str,
            #[shared_per_vcl] vp_vcl: &mut Option<Box<BgThread>>,
            /// Lambda function name or ARN
            function_name: &str,
            /// AWS region (e.g., "us-east-1")
            region: &str,
            /// Optional custom endpoint URL (e.g., for LocalStack)
            endpoint_url: Option<&str>,
        ) -> Self {
            // Create the Lambda client with the specified region and optional endpoint
            let endpoint_url_opt = endpoint_url.map(String::from);

            // Use the BgThread's runtime to initialize the AWS client
            // This ensures the client is created in the same runtime context it will be used in
            let bg = vp_vcl.as_ref().expect("BgThread not initialized");
            let region_obj = Region::new(region.to_string());
            let endpoint_for_client = endpoint_url_opt.clone();
            let client = bg.rt.block_on(async move {
                // For testing with mock endpoints, we need to configure the SDK to allow HTTP
                let sdk_config = if endpoint_for_client.is_some() {
                    // Mock/test configuration: use dummy credentials and allow HTTP
                    aws_config::defaults(aws_config::BehaviorVersion::latest())
                        .region(region_obj.clone())
                        .credentials_provider(Credentials::new(
                            "test", "test", None, None, "test"
                        ))
                        .load()
                        .await
                } else {
                    // Production configuration: use real credentials from environment
                    aws_config::from_env()
                        .region(region_obj.clone())
                        .load()
                        .await
                };

                let mut lambda_config = LambdaConfigBuilder::from(&sdk_config);
                if let Some(url) = endpoint_for_client {
                    lambda_config = lambda_config.endpoint_url(url);
                }
                LambdaClient::from_conf(lambda_config.build())
            });

            backend {
                function_name: function_name.to_string(),
                region: region.to_string(),
                client,
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

            let (tx, rx) = std_mpsc::channel();
            bg.sender.send((req, tx))?;

            // Block until Lambda responds
            let result = rx.recv()??;

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
