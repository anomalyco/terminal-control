---
"@kitlangton/terminal-control": patch
---

Avoid cloning cached terminal frames for internal text/status inspection and duplicate recording states. Borrow unedited video states instead of copying the entire timeline, and borrow socket paths during named-session request polling. Public captures remain independent owned snapshots; terminal behavior, recording formats, and rendered output are unchanged.
