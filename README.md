# libvmod-lambda

A Varnish VMOD for invoking AWS Lambda functions from VCL. Built using varnish-rs and the AWS SDK.

## Overview

This VMOD allows Varnish to invoke AWS Lambda functions as if they were backends.

## Building

```bash
cargo build --release
```

## Testing

```bash
cargo test
```

## AWS Credentials

The VMOD uses the standard AWS SDK credential chain:
- Environment variables (`AWS_ACCESS_KEY_ID`, `AWS_SECRET_ACCESS_KEY`)
- EC2 instance IAM role
- ECS task IAM role
- Shared credentials file (`~/.aws/credentials`)

No explicit credential configuration is needed if running on AWS infrastructure with IAM roles.

## Usage in VCL

### Direct Invocation Example

```vcl
import lambda;

sub vcl_init {
    new fn = lambda.backend(
        function_name="my-lambda-function",
        region="us-east-1"
    );
}

sub vcl_recv {
    # Invoke Lambda with a JSON payload
    set req.http.X-Lambda-Response = fn.invoke(
        "{\"url\": \"https://example.com\", \"viewport\": \"1920x1080\"}"
    );

    return (synth(200));
}
```

### Backend Example

Use Lambda as a Varnish backend to proxy HTTP requests:

```vcl
import lambda;

# Note that you pay for these lambda invocations, so make health check
# fast and adjust interval accordingly
probe lambda_probe {
    .url = "/ok";
    .timeout = 3s;
    .interval = 5s;
    .window = 5;
    .threshold = 3;
}

sub vcl_init {
    new my_lambda = lambda.backend(
        function_name="my-lambda",
        region="us-east-2",
        probe=lambda_probe
    );
}

sub vcl_backend_fetch {
    set bereq.backend = my_lambda.backend();
}
```

## API Reference

### lambda.backend()

Creates a new Lambda backend object.

**Parameters:**
- `function_name` (STRING, required): Lambda function name or ARN
- `region` (STRING, required): AWS region (e.g., "us-east-1", "eu-west-1")
- `endpoint_url` (STRING, optional): Custom endpoint URL (e.g., for LocalStack)
- `timeout` (DURATION, optional): Lambda invocation timeout in seconds (default: 62s)
- `probe` (PROBE, optional): Health probe configuration
- `raw_response_mode` (BOOL, optional): Whether to expect raw HTTP responses instead of JSON (default: false)

**Returns:** Backend object

**Example:**
```vcl
sub vcl_init {
    new my_function = lambda.backend(
        function_name="my-function",
        region="us-east-1",
        timeout=30s
    );
}
```

### backend.invoke()

Invokes the Lambda function with a payload.

**Parameters:**
- `payload` (STRING): JSON payload to send to Lambda

**Returns:** STRING - Response body from Lambda

**Example:**
```vcl
set req.http.X-Response = my_function.invoke('{"key": "value"}');
```

**Note:** This is a synchronous invocation (RequestResponse type). The VCL execution blocks until Lambda returns or times out.

### backend.get_function_name()

Returns the Lambda function name.

**Returns:** STRING

### backend.get_region()

Returns the AWS region.

**Returns:** STRING

### backend.backend()

Returns a VCL backend that can be used with `set bereq.backend` for proxying HTTP requests through Lambda.

**Returns:** VCL_BACKEND

**Example:**
```vcl
sub vcl_backend_fetch {
    set bereq.backend = my_lambda.backend();
}
```

## Architecture

### Async Runtime

The VMOD uses a Tokio runtime in the background to handle concurrent Lambda invocations:

- Single Tokio runtime per VCL load
- Unbounded MPSC channel for request queueing
- Each Lambda invocation runs as a separate async task
- VCL threads block on oneshot channels waiting for responses

### Timeouts

Lambda functions have a maximum timeout of 15 minutes. However, synchronous invocations should complete much faster (typically < 30 seconds) to avoid blocking Varnish worker threads.

### Lambda Payload Limits

AWS Lambda has some important limits to know about:

- Maximum synchronous payload: 6 MB
- Maximum response size: 6 MB

## Requirements

- Rust 1.91+
- Varnish 7.7+
- AWS credentials configured

## License

BSD-3-Clause
