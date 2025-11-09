# libvmod-lambda

A Varnish VMOD written in Rust using varnish-rs.

## Building

```bash
cargo build --release
```

## Testing

```bash
cargo test
```

## Example Functions

### lambda.is_even(INT) -> BOOL

Returns true if the number is even.

### lambda.double(INT) -> INT

Returns the number doubled.

### lambda.repeat(STRING) -> STRING

Returns the string concatenated with itself.

## Usage in VCL

```vcl
import lambda;

sub vcl_recv {
    if (lambda.is_even(42)) {
        # number is even
    }

    set req.http.X-Double = lambda.double(10);
    set req.http.X-Repeat = lambda.repeat("hello");
}
```
