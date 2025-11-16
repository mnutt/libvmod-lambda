vcl 4.1;

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
        function_name="test-function",
        region="us-east-2",
        probe=lambda_probe
    );
}

sub vcl_backend_fetch {
    set bereq.backend = my_lambda.backend();
}
