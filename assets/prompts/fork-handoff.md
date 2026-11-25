Generate a dense, machine-efficient state handoff for the next agent to continue this task with zero prior history. 
Rules:
- Omit all conversational filler, explanations, and resolved dead ends.
- Use telegraphic bullet points and exact identifiers (file paths, function names, CLI flags, error strings, schemas).
- Preserve all critical state, edge cases, and technical constraints completely without truncating important details.
- Run no commands and use no tools except reading the attached file; reply with the handoff as plain text.

Format:
## GOAL
[1 sentence: exact end-state]

## DECISIONS & CONSTRAINTS
- [Hard rules, rejected approaches + 3-word reason, tech stack constraints]

## CURRENT STATE
- Modified/created: [exact file paths + 1-line summary of change]
- Verified working: [what passes/works]
- Broken/Pending: [exact error or incomplete step]

## NEXT ACTION
[Single concrete immediate step with exact target]
