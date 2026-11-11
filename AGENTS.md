# ComfyFleet Project Instructions

## UI copy: no small explanatory text

The user has explicitly and permanently forbidden small explanatory text in the UI.

- Never add helper text, hint paragraphs, captions, section subtitles, or muted explanations under labels, controls, fields, dialogs, or settings rows.
- This includes `.hint`, `.hint-show`, `.field-hint`, `.help-text`, placeholder sentences, and copy such as “默认……” / “切换时……” / “保存后……”.
- Only use: labels, values, states, warnings, errors, and necessary destructive-action consequences.
- If a control seems to need a small explanation, simplify the control or remove the explanation. Do not add a second line of small text.
- When editing a UI area, remove the small explanatory text you touch rather than adding more. Do not do a broad removal sweep unless the user asks for one.
- This rule overrides any design suggestion to add a hint. The existing `DESIGN.md` “no small text under fields” rule is not optional.
