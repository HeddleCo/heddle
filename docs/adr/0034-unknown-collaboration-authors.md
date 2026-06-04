# Unknown collaboration authors

Heddle may accept collaboration operations with unknown authors locally as degraded records, but the unknown attribution is visible, low-trust, and may be rejected, quarantined, or accepted with degraded trust by Weft policy. This preserves offline usefulness without presenting unauthored discussion or context changes as equally trustworthy.

Unknown-author operations create attention items only when they affect current work, hosted sync, or policy-sensitive records. Otherwise they remain visible as low-trust provenance without turning every imported or local-only record into inbox noise.

**Status:** proposed

**Considered Options:** Requiring configured identity for every local collaboration operation would improve audit quality, but it would break local-first workflows before identity is configured. Treating unknown authors as normal would weaken provenance and hosted policy enforcement.
