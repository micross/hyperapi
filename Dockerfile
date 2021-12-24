FROM rust:alpine as build
RUN apk add --no-cache -U musl-dev protoc
WORKDIR /app
COPY . .
RUN cargo build --release

FROM alpine
WORKDIR /app
COPY --from=build /app/target/release/hyperapi ./hyperapi
ADD config.yml /app/config/default.yml
EXPOSE 80
ENTRYPOINT ["./hyperapi" ]
CMD ["--listen=0.0.0.0:80", "--config=/app/config/default.yml"]
