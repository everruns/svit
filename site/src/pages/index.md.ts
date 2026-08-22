export const prerender = true;

export function GET() {
  const markdown = `---
title: "Svit"
description: "A research-stage Rust runtime for durable agent state and reusable code."
---

# Svit

Svit keeps structured memory, named Svit Lisp scripts, inbox state, buffered
message intents, and runtime metadata in one serializable process. An activation
commits one complete next version or commits nothing.

Status: research-stage. There is no stable release, and Svit is not a proven
hostile multi-tenant isolation boundary.

## Documents

- [Overview](https://svit.everruns.com/overview/index.md)
- [Vision](https://svit.everruns.com/vision/index.md)
- [Control protocol](https://svit.everruns.com/control-protocol/index.md)
- [Security](https://svit.everruns.com/security/index.md)
- [Changelog](https://svit.everruns.com/changelog/index.md)
- [Source](https://github.com/everruns/svit)

Full documentation index: https://svit.everruns.com/llms.txt
`;

  return new Response(markdown, {
    headers: { "Content-Type": "text/markdown; charset=utf-8" },
  });
}
