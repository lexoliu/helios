## Kernel PGO

Baseline is the plain release kernel of this commit; candidate is the same
kernel built with `-C profile-use` from this run's `helios-kernel-profdata`,
which the `profile-generate` job collected from an instrumented boot of the
same commit (`docs/pgo.md`). Both images were built here and booted back to
back for every workload on this runner, so what separates the two columns is
profile-guided optimisation and nothing else.

Read the headline compute workloads first — `aot-curl`, `cpython-json`,
`quickjs-loop` — because they are what the profile covers. The `net`
workloads are timed on both images but are **not represented in the
profile**: the collection job runs the classes that need no privileged host
networking, so the candidate's packet path carries no counts and its rows say
what an unprofiled path does under a PGO build, not what PGO does to it.

This comparison reports and does not gate. Its verdict is about
profile-guided optimisation of the kernel, not about the pull request that
happened to run it, so a red headline row here says PGO did not pay on this
commit — which is the answer the job exists to produce.
