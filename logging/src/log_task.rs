// The log task should be able to take in arbitrary channels, handle serialization, and write to disk.
//
// The complicated part will be the introspection on the pub_sub system where we need to create subscribers for this task based on
// all the tasks that exist in the graph.
//
// So maybe this'll be a connection step after all user tasks are connected. Some task builder could order a bunch of steps in case we have
// other infrastructure-y steps.
