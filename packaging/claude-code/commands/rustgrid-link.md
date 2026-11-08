---
description: Link this Claude Code session to a RustGrid ticket so it is traced there and its content is loaded
argument-hint: TICKET-KEY (for example ADD-354)
---

RustGrid's hook links this session to the ticket `$ARGUMENTS` before this
prompt reaches you. It adds context starting "RustGrid linked this Claude Code
session", with the ticket's title, description, and the local paths of its
attachments. Follow the instructions at the end of that context: confirm the
link to the user in one short sentence, naming the ticket key and title, and
do nothing else until the user asks. Keep the ticket's content in mind for the
work that follows, and read its attachments when they matter.

If there is no such line, RustGrid tracing is not installed on this computer:
tell the user to run `rustgrid-agent trace install`, and do nothing else.
