# One imported Git tip per result slot

The owner decision of 2026-10-05 resolves the earlier carrier question.
An import publishes one native Capture per branch result slot. Converted Git
ancestors are State objects in the tip's content closure, not native operations.

The opaque `DelegatedImport` is the discriminator. Construction verifies the
job signature under an authenticated delegation, exact Thread/genesis and
account scope, the complete causal frontier, the native operation ID and the
Capture content commitment. Its private fields prevent a caller flag or an
unrelated operation's carrier from selecting the exception.

With an empty causal frontier, the converted State retains its ordered Git
parents, including none for a Git root. Their fidelity remains attested by the
shared converter and signed host conversion. The canonical synthetic base is
required and cannot appear as a Git parent. A nonempty causal frontier retains
exact native source-parent equality. Duplicate State parents reject.

Standalone `ThreadOperation::validate_parents` remains strict. Ordinary native
Captures and LocalIntegration retain their existing rules. State IDs, the
converter and signing formats are unchanged. The retired HostedImport arm is
not used to admit this exception. See the Part 5 evidence for the path audit.
