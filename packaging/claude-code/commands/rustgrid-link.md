---
description: Link this Claude Code session to a RustGrid ticket so it is traced there
argument-hint: TICKET-KEY (for example ADD-354)
---

RustGrid's `UserPromptSubmit` hook handles `/rustgrid-link $ARGUMENTS` before
this prompt reaches you, and answers the user directly. If you are reading
this, the hook did not run. Tell the user that RustGrid tracing is not
installed on this computer: `rustgrid-agent` 1.6.0 or later and the RustGrid
managed settings are required. Do nothing else.
