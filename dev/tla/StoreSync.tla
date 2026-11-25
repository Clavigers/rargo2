---------------------------- MODULE StoreSync ----------------------------


EXTENDS Naturals, FiniteSets

CONSTANTS Req,
          Blob,
          Attempts,
          CleanupOnCancel

None == 0

VARIABLES
    present,
    pins,
    uploading,
    nextTok,
    pc,
    waitTok,
    mine,
    tries,
    held

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

Walk(r) ==
    /\ pc[r] = "walk"
    /\ tries[r] < Attempts
    /\ LET missing == {b \in Blob : ~present[b]}
           free    == {b \in missing : uploading[b] = None}
           busy    == missing \ free
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

Sent(r, ok) ==
    /\ pc[r] = "send"
    /\ present' = [b \in Blob |-> present[b] \/ (ok /\ b \in mine[r])]
    /\ uploading' = [b \in Blob |-> IF b \in mine[r] THEN None ELSE uploading[b]]
    /\ mine' = [mine EXCEPT ![r] = {}]
    /\ pc' = [pc EXCEPT ![r] = IF ~ok THEN "failed"
                               ELSE IF \E b \in Blob : waitTok[r][b] # None THEN "wait" ELSE "hold"]
    /\ UNCHANGED <<pins, nextTok, waitTok, tries, held>>

Waited(r) ==
    /\ pc[r] = "wait"
    /\ LET w == {b \in Blob : waitTok[r][b] # None}
       IN /\ \A b \in w : present[b] \/ uploading[b] # waitTok[r][b]
          /\ waitTok' = [waitTok EXCEPT ![r] = [b \in Blob |-> None]]
          /\ pc' = [pc EXCEPT ![r] = IF \A b \in w : present[b] THEN "hold" ELSE "walk"]
    /\ UNCHANGED <<present, pins, uploading, nextTok, mine, tries, held>>

Hold(r) ==
    /\ pc[r] = "hold"
    /\ IF \A b \in Blob : present[b]
       THEN /\ pins' = [b \in Blob |-> pins[b] + 1]
            /\ held' = [held EXCEPT ![r] = TRUE]
            /\ pc' = [pc EXCEPT ![r] = "done"]
       ELSE /\ pc' = [pc EXCEPT ![r] = "walk"]
            /\ UNCHANGED <<pins, held>>
    /\ UNCHANGED <<present, uploading, nextTok, waitTok, mine, tries>>

Release(r) ==
    /\ pc[r] = "done" /\ held[r]
    /\ pins' = [b \in Blob |-> pins[b] - 1]
    /\ held' = [held EXCEPT ![r] = FALSE]
    /\ UNCHANGED <<present, uploading, nextTok, pc, waitTok, mine, tries>>

Cancel(r) ==
    /\ pc[r] \in {"walk", "send", "wait", "hold"}
    /\ pc' = [pc EXCEPT ![r] = "cancelled"]
    /\ IF CleanupOnCancel
       THEN /\ uploading' = [b \in Blob |-> IF b \in mine[r] THEN None ELSE uploading[b]]
            /\ mine' = [mine EXCEPT ![r] = {}]
       ELSE UNCHANGED <<uploading, mine>>
    /\ UNCHANGED <<present, pins, nextTok, waitTok, tries, held>>

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

StateBound == nextTok <= 14


HeldIsPresent == \A r \in Req : held[r] => \A b \in Blob : present[b]

SyncEnds == \A r \in Req : <>(pc[r] \in {"done", "failed", "cancelled"})

=============================================================================
