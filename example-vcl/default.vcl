vcl 4.1;

import lambda;

backend default {
    .host = "127.0.0.1";
    .port = "8080";
}

sub vcl_init {
    new my_lambda = lambda.backend(
        function_name="test-function",
        region="us-east-2",
        endpoint_url=""
    );
}

sub vcl_recv {
    if (req.url == "/test") {
        set req.http.X-Lambda-Response = my_lambda.invoke({"{"test": true}"});
        return (synth(200));
    }
}

sub vcl_synth {
    if (resp.status == 200 && req.http.X-Lambda-Response) {
        set resp.http.Content-Type = "application/json";
        synthetic(req.http.X-Lambda-Response);
        return (deliver);
    }
}
