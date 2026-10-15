"""Multi-persona long-conversation synthetic dataset generator (batch layer).

Thin Python driver around the `loop-conversation-gen` Rust binary — mirrors
`datagen/`'s checkpoint/curation/manifest conventions, simplified (no
verdict/verification concept; a conversation is just "completed" or "error").
"""
