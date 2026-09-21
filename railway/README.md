# Railway engine-dev service

`engine-dev.Dockerfile` builds a worker with the pinned Rust toolchain
(1.93.0), Python and maturin 1.15.0, the same as the Linux wheel build. The
service is connected to this repository (branch `claude/railway-engine-dev`
for the infrastructure files; switch the service's branch to the engine
branch under test) and mounts a volume at `/data` for the cargo target
cache, cargo home, private workbooks and built wheels. It runs no server;
work happens over `railway ssh`. Private workbooks are copied in over ssh
stdin and never committed.
