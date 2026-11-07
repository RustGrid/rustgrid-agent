---
description: Link this Claude Code session to a RustGrid ticket so it is traced there
argument-hint: TICKET-KEY (for example ADD-354)
---

RustGrid's hook links this session to the ticket `$ARGUMENTS` before this
prompt reaches you, and adds a line starting "RustGrid linked this Claude Code
session" to the context. Confirm the link to the user in one short sentence,
naming the ticket key and title, and do nothing else.

If there is no such line, RustGrid tracing is not installed on this computer:
tell the user to run `rustgrid-agent trace install`, and do nothing else.
