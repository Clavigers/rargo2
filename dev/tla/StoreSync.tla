---------------------------- MODULE StoreSync ----------------------------
(* The server side of a worktree sync, as `Trees::sync`, `Trees::upload`,
   `Trees::hold` and `Store::{pin_all, evict}` do it (crates/rargo-core/src/
   snapshot.rs, store.rs).

   Requests that need the same file content share one upload: the first
   claims it in `uploading`, the others wait until the content is in the
   store or the claim is gone. A sync then holds its snapshot: checks that
   the store has everything and pins it, in one step with respect to
   eviction (`pin_gate`). Eviction runs at any time and removes what is
   not pinned. A request can be cancelled at any point (the client hangs
   up, `ClientMsg::Cancel`, the daemon's link drops): its task is aborted,
   which drops the sync future wherever it is suspended.

   `CleanupOnCancel` says whether dropping the future gives up the claim.
   The code does (`Claims` releases it on drop): StoreSync_TRUE.cfg. With
   FALSE, TLC finds a waiter that waits forever on the claim of a request
   that is gone: StoreSync_FALSE.cfg, the reason the guard exists.  *)
EXTENDS Naturals, FiniteSets

CONSTANTS Req,              \* the requests
          Blob,             \* file contents the worktree needs
          Attempts,         \* SYNC_ATTEMPTS
          CleanupOnCancel   \* does an aborted upload release its claim?

None == 0

VARIABLES
    present,    \* Blob -> BOOLEAN: Store::has
    pins,       \* Blob -> Nat: Store::pins
    uploading,  \* Blob -> claim token (0: unclaimed): Trees::uploading
    nextTok,    \* fresh Notify identities, for Arc::ptr_eq
    pc,         \* Req -> state of the request's sync
    waitTok,    \* Req -> Blob -> the claim a waiter waits on
    mine,       \* Req -> SUBSET Blob claimed by this request
    tries,      \* Req -> attempts used
    held        \* Req -> BOOLEAN: holds a pinned snapshot

vars == <<present, pins, uploading, nextTok, pc, waitTok, mine, tries, held>>

States == {"walk", "send", "wait", "hold", "done", "failed", "cancelled"}

TypeOK ==
    /\ present \in [Blob -> BOOLEAN]
    /\ pins \in [Blob -> Nat]
    /\ uploading \in [Blob -> Nat]
    /\ pc \in [Req -> States]
    /\ mine \in [Req -> SUBSET Blob]
    /\ held \in [Req -> BOOLEAN]

Init ==
    /\ present = [b \in Blob |-> FALSE]
    /\ pins = [b \in Blob |-> 0]
    /\ uploading = [b \in Blob |-> None]
    /\ nextTok = 1
    /\ pc = [r \in Req |-> "walk"]
    /\ waitTok = [r \in Req |-> [b \in Blob |-> None]]
    /\ mine = [r \in Req |-> {}]
    /\ tries = [r \in Req |-> 0]
    /\ held = [r \in Req |-> FALSE]

(* The walk found these missing; under the `uploading` lock each is
   either claimed or waited on (one step: the lock). *)
Walk(r) ==
    /\ pc[r] = "walk"
    /\ tries[r] < Attempts
    /\ LET missing == {b \in Blob : ~present[b]}
           free    == {b \in missing : uploading[b] = None}
           busy    == missing \ free
           \* fresh tokens for each claim, in some fixed order
           tok     == CHOOSE f \in [free -> nextTok .. nextTok + Cardinality(free)] :
                          \A x, y \in free : x # y => f[x] # f[y]
       IN /\ uploading' = [b \in Blob |-> IF b \in free THEN tok[b] ELSE uploading[b]]
          /\ nextTok' = nextTok + Cardinality(free) + 1
          /\ mine' = [mine EXCEPT ![r] = free]
          /\ waitTok' = [waitTok EXCEPT ![r] = [b \in Blob |-> IF b \in busy THEN uploading[b] ELSE None]]
          /\ tries' = [tries EXCEPT ![r] = @ + 1]
          /\ pc' = [pc EXCEPT ![r] = IF free # {} THEN "send"
                                     ELSE IF busy # {} THEN "wait" ELSE "hold"]
    /\ UNCHANGED <<present, pins, held>>

GiveUp(r) ==
    /\ pc[r] = "walk" /\ tries[r] >= Attempts
    /\ pc' = [pc EXCEPT ![r] = "failed"]
    /\ UNCHANGED <<present, pins, uploading, nextTok, waitTok, mine, tries, held>>

(* `from.blobs(mine)` returns (the client sent them, or the call failed),
   and the claims are removed and their waiters notified. *)
Sent(r, ok) ==
    /\ pc[r] = "send"
    /\ present' = [b \in Blob |-> present[b] \/ (ok /\ b \in mine[r])]
    /\ uploading' = [b \in Blob |-> IF b \in mine[r] THEN None ELSE uploading[b]]
    /\ mine' = [mine EXCEPT ![r] = {}]
    /\ pc' = [pc EXCEPT ![r] = IF ~ok THEN "failed"
                               ELSE IF \E b \in Blob : waitTok[r][b] # None THEN "wait" ELSE "hold"]
    /\ UNCHANGED <<pins, nextTok, waitTok, tries, held>>

(* A waiter moves on once the content is there or the claim it waited on
   is gone (Arc::ptr_eq on the Notify). Until then it sleeps and looks
   again: a step that changes nothing, so it is not an action here. *)
Waited(r) ==
    /\ pc[r] = "wait"
    /\ LET w == {b \in Blob : waitTok[r][b] # None}
       IN /\ \A b \in w : present[b] \/ uploading[b] # waitTok[r][b]
          /\ waitTok' = [waitTok EXCEPT ![r] = [b \in Blob |-> None]]
          \* what the other uploader failed to bring is ours next round
          /\ pc' = [pc EXCEPT ![r] = IF \A b \in w : present[b] THEN "hold" ELSE "walk"]
    /\ UNCHANGED <<present, pins, uploading, nextTok, mine, tries, held>>

(* Trees::hold via Store::pin_all: check and pin in one step (pin_gate). *)
Hold(r) ==
    /\ pc[r] = "hold"
    /\ IF \A b \in Blob : present[b]
       THEN /\ pins' = [b \in Blob |-> pins[b] + 1]
            /\ held' = [held EXCEPT ![r] = TRUE]
            /\ pc' = [pc EXCEPT ![r] = "done"]
       ELSE /\ pc' = [pc EXCEPT ![r] = "walk"]
            /\ UNCHANGED <<pins, held>>
    /\ UNCHANGED <<present, uploading, nextTok, waitTok, mine, tries>>

(* The build finishes and the snapshot is dropped: unpin_all. *)
Release(r) ==
    /\ pc[r] = "done" /\ held[r]
    /\ pins' = [b \in Blob |-> pins[b] - 1]
    /\ held' = [held EXCEPT ![r] = FALSE]
    /\ UNCHANGED <<present, uploading, nextTok, pc, waitTok, mine, tries>>

(* The request's task is aborted wherever it is suspended. *)
Cancel(r) ==
    /\ pc[r] \in {"walk", "send", "wait", "hold"}
    /\ pc' = [pc EXCEPT ![r] = "cancelled"]
    /\ IF CleanupOnCancel
       THEN /\ uploading' = [b \in Blob |-> IF b \in mine[r] THEN None ELSE uploading[b]]
            /\ mine' = [mine EXCEPT ![r] = {}]
       ELSE UNCHANGED <<uploading, mine>>
    /\ UNCHANGED <<present, pins, nextTok, waitTok, tries, held>>

(* Store::evict: under pin_gate's write side, skip what is pinned. *)
Evict(b) ==
    /\ present[b] /\ pins[b] = 0
    /\ present' = [present EXCEPT ![b] = FALSE]
    /\ UNCHANGED <<pins, uploading, nextTok, pc, waitTok, mine, tries, held>>

Next ==
    \/ \E r \in Req : Walk(r) \/ GiveUp(r) \/ Sent(r, TRUE) \/ Sent(r, FALSE)
                      \/ Waited(r) \/ Hold(r) \/ Release(r) \/ Cancel(r)
    \/ \E b \in Blob : Evict(b)

Fairness ==
    \A r \in Req : /\ WF_vars(Walk(r)) /\ WF_vars(GiveUp(r))
                   /\ WF_vars(Sent(r, TRUE) \/ Sent(r, FALSE))
                   /\ WF_vars(Waited(r)) /\ WF_vars(Hold(r))

Spec == Init /\ [][Next]_vars /\ Fairness

\* Bound the state space: tokens only grow.
StateBound == nextTok <= 14

(* --- Properties --- *)

\* What a running build holds is in the store.
HeldIsPresent == \A r \in Req : held[r] => \A b \in Blob : present[b]

\* A request that is not cancelled ends: it holds its snapshot or fails
\* with a message. It never waits forever.
SyncEnds == \A r \in Req : <>(pc[r] \in {"done", "failed", "cancelled"})

=============================================================================
