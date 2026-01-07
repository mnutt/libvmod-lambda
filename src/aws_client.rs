//! AWS Lambda client creation and configuration.
//!
//! This module handles building Lambda clients with the appropriate
//! configuration for credentials, region, endpoint, and retry behavior.

use aws_sdk_lambda::Client as LambdaClient;
use aws_sdk_lambda::config::Builder as LambdaConfigBuilder;
use aws_sdk_lambda::config::retry::RetryConfig;
use aws_credential_types::Credentials;
use aws_types::region::Region;

/// Build a Lambda client with the specified configuration.
///
/// # Configuration
/// - For custom endpoints (e.g., LocalStack): uses credentials from environment variables
/// - For production: uses standard AWS credential chain
/// - Retries are disabled to let Varnish handle retry logic at a higher level
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

    // Disable SDK retries - let Varnish handle retry logic at a higher level.
    // This prevents worker threads from being blocked during exponential backoff
    // and ensures metrics (inflight, throttled, fail) are accurate.
    let retry_config = RetryConfig::standard().with_max_attempts(1);

    let mut lambda_config = LambdaConfigBuilder::from(&sdk_config)
        .retry_config(retry_config);
    if let Some(url) = endpoint_url {
        lambda_config = lambda_config.endpoint_url(url);
    }
    LambdaClient::from_conf(lambda_config.build())
}
