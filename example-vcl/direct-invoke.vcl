vcl 4.1;

sub vcl_recv {
    # Example: Directly invoke lambda with a JSON payload for specific endpoint
    # This is useful when you want to call Lambda as a function
    # rather than proxying HTTP requests to it
    if (req.url == "/test-endpoint") {
        # Build a JSON payload from request parameters
        set req.http.X-Lambda-Payload = {"
{
    "message": "Hello from Varnish",
    "path": "} + req.url + {"",
    "method": "} + req.method + {""
}
"};

        # Invoke the lambda function synchronously
        set req.http.X-Lambda-Response = my_lambda.invoke(req.http.X-Lambda-Payload);

        # Return synthetic response with lambda result
        return (synth(700, "Lambda invoke"));
    }
}

sub vcl_synth {
    if (resp.status == 700) {
        set resp.status = 200;
        set resp.http.Content-Type = "application/json";
        synthetic(req.http.X-Lambda-Response);
        return (deliver);
    }
}
