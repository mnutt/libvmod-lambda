vcl 4.1;

backend default none;

# Include backend setup (creates lambda backend with health probe)
include "/etc/varnish/backend.vcl";

# Include direct invocation logic for /test-endpoint
include "/etc/varnish/direct-invoke.vcl";

sub vcl_recv {
    # For all other routes, pass through to backend
    return (pass);
}
