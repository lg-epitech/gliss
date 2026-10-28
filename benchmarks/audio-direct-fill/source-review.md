# Independent source review and disposition

28 October 2026 · T3 delegate, Claude Fable 5.1, high effort · source inspection only

The reviewer inspected the isolated player, its derivation/sample tests, the
browser page, original controller, plan and two archived failed setup reports.
It found no player correctness issue. Writes through `getChannelData()` finish
before the buffer is assigned to a source; stale output, mute and queue bounds
retain the original checks. No reviewed source changed during the review.

The review also identified limits in the controller. The later 256-task source
was not part of that review. Its original controller and page behavior remain
the same, with separate resource admission checks.

- **Abort after an invalid position:** an invalid measurement stops the remaining
  positions. The report retains those slots as blocked, and a partial cohort
  cannot satisfy the retention criteria. All eight final positions completed;
  no missing positions contributed to the rejection decision. Future controllers
  should retain partial progress durably and decide whether another launch is
  safe from confirmed cleanup, rather than assuming either outcome.
- **Fixed-window edge:** the last packet is planned at 5,980 ms. Both its send
  and decoded scheduling must finish inside the 6,000 ms window, so it has less
  headroom than the general 100 ms submission ceiling. The gates compose;
  100 ms is not guaranteed tolerance at the cutoff. All 300 sends and schedule
  callbacks in every final position were observed inside the window. The proposed
  post-window drain was not adopted: accepting tail work would change the fixed
  observation contract after registration.
- **Termination durability:** an owner kill can bypass the controller's final
  JSON write. This remains a runner limitation. Both setup failures and the
  final cohort exited before their owners' limits and retained complete reports;
  no killed run is presented as a measurement. The discarded experiment is not
  a general fault-tolerant benchmark framework.
- **Error preservation:** a failed page cleanup can mask a measurement exception;
  global page errors also conservatively invalidate later positions. Neither
  occurred in the final cohort. Its page-error array is empty. Phase labels
  localize setup failures; private stack logs are hashed, not published.
- **Retention sensitivity:** the reviewer questioned whether eliminating small
  copies can move whole-browser CPU by 10%. That threshold was intentional and
  remained fixed. Secondary scheduling timings do not replace it after the
  outcome is known.

The resource hypothesis was tested separately. The 128-task probe recorded
three task-denial events; the identical setup passed at 256 tasks, peaked at
132 and recorded none. The final cost cohort peaked at 133 and recorded no
task or memory denials. These are measured own-cgroup values; the reviewer's
rough component thread estimates are not used. This establishes that the first
resource ceiling was insufficient, without tracing each original timeout to
an individual failed thread creation.

The first archived run proved PCM equality only; the diagnostic run and final
cohort proved both PCM and Opus. Exact pre-playback buffer samples are not a
speaker, listening-quality or rendered-output test. The review performed no
runtime checks and supplies no performance approval.
