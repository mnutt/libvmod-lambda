# libvmod-lambda

A Varnish VMOD for invoking AWS Lambda functions from VCL. Built with Rust using varnish-rs and the AWS SDK.

## Overview

This VMOD allows Varnish to invoke AWS Lambda functions as if they were backends. It's designed for high-throughput scenarios (10,000+ req/s) with thousands of concurrent invocations.

**Use Case:** Invoke Lambda functions for tasks like:
- Headless browser rendering (WebKit, Chromium)
- Image transformation and optimization
- Dynamic content generation
- Authentication and authorization
- API gateway/proxy patterns

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

### Basic Example

```vcl
import lambda;

sub vcl_init {
    new renderer = lambda.backend(
        function_name="my-renderer-function",
        region="us-east-1"
    );
}

sub vcl_recv {
    # Invoke Lambda with a JSON payload
    set req.http.X-Lambda-Response = renderer.invoke(
        "{\"url\": \"https://example.com\", \"viewport\": \"1920x1080\"}"
    );
}
```

### Backend Integration Example

```vcl
import lambda;
import std;

sub vcl_init {
    new screenshot = lambda.backend(
        function_name="webkit-screenshot",
        region="us-west-2"
    );
}

sub vcl_recv {
    if (req.url ~ "^/render/") {
        # Extract URL from path
        set req.http.X-Target-URL = regsub(req.url, "^/render/", "");

        # Build Lambda payload
        set req.http.X-Payload = "{" +
            {"url": "} + req.http.X-Target-URL + {"", } +
            {"format": "png", } +
            {"width": 1920, } +
            {"height": 1080"} +
        "}";

        # Invoke Lambda
        set req.http.X-Image-Data = screenshot.invoke(req.http.X-Payload);

        return (synth(200));
    }
}

sub vcl_synth {
    if (resp.status == 200 && req.http.X-Image-Data) {
        set resp.http.Content-Type = "image/png";
        synthetic(req.http.X-Image-Data);
        return (deliver);
    }
}
```

## API Reference

### lambda.backend()

Creates a new Lambda backend object.

**Parameters:**
- `function_name` (STRING): Lambda function name or ARN
- `region` (STRING): AWS region (e.g., "us-east-1", "eu-west-1")

**Returns:** Backend object

**Example:**
```vcl
sub vcl_init {
    new my_function = lambda.backend(
        function_name="my-function",
        region="us-east-1"
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

## Architecture

### Async Runtime

The VMOD uses a Tokio runtime in the background to handle concurrent Lambda invocations:

- Single Tokio runtime per VCL load
- Unbounded MPSC channel for request queueing
- Each Lambda invocation runs as a separate async task
- VCL threads block on oneshot channels waiting for responses

### Scalability

Designed to handle thousands of concurrent Lambda invocations:

- **Connection Pooling:** AWS SDK client uses HTTP/2 connection pooling
- **Async I/O:** No thread-per-request overhead
- **Concurrent Execution:** Tokio multiplexes thousands of tasks on a single runtime
- **Non-blocking VCL:** Worker threads queue requests and return to VCL processing

Expected bottleneck is Lambda account concurrency limit (default 1000, can be increased to 10,000+), not the VMOD.

### Error Handling

Lambda errors are propagated back to VCL as exceptions. VCL can handle failures with standard error handling:

```vcl
if (req.http.X-Response) {
    # Success
} else {
    # Lambda failed - fallback to default backend
    set req.backend_hint = default;
}
```

## Performance Considerations

### Timeouts

Lambda functions have a maximum timeout of 15 minutes. However, synchronous invocations should complete much faster (typically < 30 seconds) to avoid blocking Varnish worker threads.

### Payload Limits

- Maximum synchronous payload: 6 MB
- Maximum response size: 6 MB

### Latency

Lambda invocation latency varies based on:
- Cold starts (first invocation or after idle period)
- Function execution time
- Network latency to Lambda endpoints

For the headless browser use case, expect p99 latencies of 5-30 seconds depending on page complexity.

## Requirements

- Rust 1.91+
- Varnish 7.7+
- AWS credentials configured

## License

BSD-3-Clause
