---
type: Service
title: Build server
description: CI runner host that syncs time from the time server
resource: https://build.example.com
tags: [ci]
status: stable
generated: { by: seed, at: 2026-09-01T00:00:00Z }
---

# Build server

`build.example.com` runs the lab's CI jobs on 8 cores. It takes its time from the [time server](/infra/time-server.md); when that drifts, signed builds fail. It resolved names through [Legacy DNS](/infra/legacy-dns.md) until 2026-06-30.
