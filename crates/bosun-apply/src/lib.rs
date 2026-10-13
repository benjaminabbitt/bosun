//! bosun drivers: build the plan from resolved versions and machine state
//! (fixed ordering, actions, exit codes, §4.8) and apply it to the host,
//! writing state only after each install verifies and isolating per-backend
//! failures (§4.8, Appendix B). The container renderer is a peer driver
//! (§8), out of M1.
