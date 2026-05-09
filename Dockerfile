FROM golang:1.22-alpine AS sidecar-builder

WORKDIR /src
COPY tls-sidecar/go.mod tls-sidecar/go.sum ./
RUN go mod download
COPY tls-sidecar/main.go ./
RUN CGO_ENABLED=0 go build -ldflags="-s -w" -o /out/tls-sidecar .

FROM node:22-alpine AS frontend-builder

# pnpm 固定 v9：v10 默认拒绝执行未审批的依赖 build 脚本（@swc/core、esbuild 等），
# CI 非交互环境直接 fail。锁 v9 避免该兼容性问题。
WORKDIR /app/admin-ui
COPY admin-ui/package.json ./
RUN npm install -g pnpm@9 && pnpm install
COPY admin-ui ./
RUN pnpm build

FROM rust:1.92-alpine AS builder

RUN apk add --no-cache musl-dev perl make

WORKDIR /app
COPY Cargo.toml Cargo.lock* ./
COPY src ./src
COPY --from=frontend-builder /app/admin-ui/dist /app/admin-ui/dist

RUN cargo build --release --no-default-features

FROM alpine:3.21

RUN apk add --no-cache ca-certificates

WORKDIR /app
COPY --from=builder /app/target/release/kiro-rs /app/kiro-rs
COPY --from=sidecar-builder /out/tls-sidecar /app/tls-sidecar

VOLUME ["/app/config"]

EXPOSE 8990

CMD ["./kiro-rs", "-c", "/app/config/config.json", "--credentials", "/app/config/credentials.json"]
