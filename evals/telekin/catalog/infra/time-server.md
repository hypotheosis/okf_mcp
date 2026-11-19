---
type: Service
title: Time server
description: NTP source for the lab, with its drift incident
resource: ntp://ntp1.example.com
aliases: [ntp1]
tags: [time, ntp]
status: stable
generated: { by: seed, at: 2026-09-01T00:00:00Z }
sources:
  - id: chrony-docs
    resource: https://chrony-project.org/documentation.html
    title: chrony documentation
---

# Time server

`ntp1.example.com` serves NTP on UDP port 123 to every host in the lab, including the [build server](/infra/build-server.md). It runs chrony.[^chrony-docs] Its owner is the [eval owner](/people/eval-owner.md).

## Drift incident

On 2026-08-14 the clock drifted 4 seconds after its upstream pool became unreachable. Builds failed signature checks until chrony was pointed at a second upstream. Normal offset is under 100 ms.

[^chrony-docs]: chrony documentation, https://chrony-project.org/documentation.html
