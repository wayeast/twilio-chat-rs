FROM clux/muslrust:1.70.0 AS builder

WORKDIR /build
COPY . .
RUN cargo build --release

#################################################

FROM alpine:3
COPY --from=builder /build/target/x86_64-unknown-linux-musl/release/twilio-rs /bin/twilio-rs

ENTRYPOINT ["twilio-rs"]
