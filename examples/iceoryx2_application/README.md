# Iceoryx2 + CFW FizzBuzz

Run `cargo run -p iceoryx2_application -- --count 30` from the repository root.

The launcher starts a **buzzer** process, waits for its stdout ready signal, and
then starts a **fizzer** process. The fizzer publishes integers on an iceoryx2
service. In the buzzer, an iceoryx2 event wakes a CFW calculator that reads
the latest integer and publishes a FizzBuzz string on a native CFW channel.
The calculator acknowledges each integer over a second iceoryx2 service; the
fizzer waits for that acknowledgment before publishing the next value. This
keeps the optional latest-value input from skipping values.
A periodic CFW callback accumulates metrics from a span of those strings, and
a printer callback displays each string with the latest available metrics.
A second metrics subscriber prints the final summary and signals completion;
the metric values stay in the CFW pub/sub graph.
The launcher closes the fizzer's stdin when the buzzer finishes so that the
iceoryx2 publisher remains alive until the final value has been received.
