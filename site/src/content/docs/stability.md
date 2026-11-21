---
title: Stability
description: What newt promises within a major version, what minor releases may add, and what is outside the promise.
---

newt follows semantic versioning. Within a major version (2.x now), nothing below changes in a way that breaks a working setup. 2.0 replaced the HTTP context provider with the home MCP server; the [changelog](../changelog/) has what changed.

## Covered

- **The event API** ([Protocol, section 5](../protocol/#5-event-api-called-by-your-application)): paths, fields, status codes.
- **The `newt` MCP extension**: the home's declaration, `newt/turn`, `newt/turn_end` and the `newt/*` `_meta` fields ([Protocol, sections 1 to 4](../protocol/)).
- **The `server.toml` keys** and their defaults ([Configuration](../configuration/)).
- **The database.** newt applies its own migrations on start, and within a major version they only add tables, columns and indexes. Upgrading is replacing the binary. Downgrading is not supported.

## What a minor release may add

Optional fields, endpoints, live feed event types and config keys. Clients should ignore fields they do not know.

## Outside the promise

- **Experimental features.** A minor release may change them, and the [changelog](../changelog/) says so. The list is under [Status](../overview/#status).
- The Rust crates, which are not published, and their APIs.
- The JSONL event file and the OTLP span attributes.
