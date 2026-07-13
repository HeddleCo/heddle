---
status: accepted
---

# Dispatch source operations by repository authority

Repository Source Authority determines which command surface owns source history. Git Overlay sends commits, pulls, and pushes directly to Git. Heddle commands in that mode mutate `.heddle` metadata, except `land`, which is the explicit local projection boundary for a managed Heddle thread. Native Heddle repositories use Heddle source transport.

`capture` is the only public Heddle save boundary. `commit`, `checkpoint`, and the publish flags on `land` are removed. Git Overlay invocations of Heddle-native `push` and `pull` refuse before mutation and return exact direct-Git argv. `adopt` is the atomic transition to the full native source-operation surface.

Behavior, recommendations, and machine action templates select typed source actions from durable authority. They do not repair invalid command strings after construction.
