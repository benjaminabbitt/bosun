//! bosun resolver: turn a manifest plus `Index` queries into resolved
//! versions, applying ordering (§6), policy (§7), and cross-claim rules
//! (§4.6: lockfiles advise, specs decide; conflicting exact pins refused).
