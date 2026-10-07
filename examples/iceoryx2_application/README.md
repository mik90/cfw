# Iceoryx2 + CFW FizzBuzz

Run from the repository root:

```sh
cargo run -p iceoryx2_application -- --count 30
```

The launcher starts two processes connected by iceoryx2:

- **Fizzer:** publishes integers, waiting for an acknowledgment before sending the next.
- **Buzzer:** runs four tasks:
  - **Calculator:** converts each integer into a FizzBuzz result and acknowledges it.
  - **Metrics recorder:** counts Fizz, Buzz, FizzBuzz, and numeric results.
  - **Printer:** prints each result with the latest metrics.
  - **Metrics summary:** prints the final totals and signals completion.

Within the buzzer, results and metrics travel over native CFW channels.
