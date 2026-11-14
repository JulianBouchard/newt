---
title: Stability
description: What newt promises within 1.x, what minor releases may add, and what is outside the promise.
---

From 1.0 newt follows semantic versioning. Within 1.x, nothing below changes in a way that breaks a working setup.

## Covered

- **The event API** ([Protocol, section 5](../protocol/#5-event-api-called-by-your-application)): paths, fields, status codes.
- **The context-provider protocol** and the `newt/*` MCP `_meta` fields ([Protocol, sections 1 to 4](../protocol/)).
- **The `server.toml` keys** and their defaults ([Configuration](../configuration/)).
- **The database.** newt applies its own migrations on start, and within 1.x they only add tables, columns and indexes. Upgrading is replacing the binary. Downgrading is not supported.

## What a minor release may add

Optional fields, endpoints, live feed event types and config keys. Clients should ignore fields they do not know.

## Outside the promise

- **Experimental features.** A minor release may change them, and the [changelog](../changelog/) says so. The list is under [Status](../overview/#status).
- The Rust crates, which are not published, and their APIs.
- The JSONL event file and the OTLP span attributes.
