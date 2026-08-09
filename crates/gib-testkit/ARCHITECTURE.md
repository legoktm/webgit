Test-only support, consumed exclusively as a dev-dependency: `TestRepo`
builds real repositories by shelling out to the `git` CLI, and a blocking
`std::fs` implementation of the `gib-fs` traits lets any crate open them.
This is the foundation of the differential test suites that compare library
output against whatever `git` binary is installed on the host.
