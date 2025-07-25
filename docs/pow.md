# Proof-of-work in the faucet

The faucet uses Proof-of-Work (POW) challenges for anti-Denial-of-Service and dynamically shifts the
difficulty based on the faucet's balance.

Generally, the less bitcoin in the faucet, the more difficult it is to get out
via solving the POW challenge.

Output: $y$, difficulty parameter as a power of 2, i.e. $2^y$. Amounts are in sats.

Parameters:
- $x$: Current balance of the faucet. Positive integer.
- $m$: Minimum difficulty parameter, a good default is about 17-20 (usually takes a couple seconds
on modern hardware in optimized code). Between 0 and $M$.
- $q$: Amount in sats emitted per successful faucet claim. >0 integer.
- $L$: Difficulty linear increase coefficient. A sane default is 10-25. >0 integer.
- $b$: Minimum balance. It will be computationally impossible to drop the faucet's balance below
this value. Defaults to 0. Positive integer.

Constants:
- $M$: Maximum difficulty parameter = 255

$$\text{base}(x) = \frac{m - M}{Lq}(x-b) + M$$

$$y = \max(m, \min(M, \text{base}(x)))$$

Derivation method:

For $x \leq b$, $y = M$. I.e, when the faucet is at or below the minimum balance, the difficulty is
maximum.

At $x \geq b + Lq$, $y = m$. When the faucet has at least $b + Lq$, the difficulty is minimum. $Lq$
was chosen as it is the period of balance that the difficulty will increase to $M$ from $m$. Having
this be proportional to $q$ makes sense because $q$ is the amount $x$ will ever shift down by during
normal faucet operations.

$L$ controls the rate at which the difficulty increases. A higher $L$ means it will increase slower,
but the difficulty will start increasing earlier.

For example, if $L = 10$, $b = 0$ and $q = 10_000$, we'll start ramping up the difficulty once the
faucet's balance drops below $b + Lq = 100_000$ sats.

Given these rules, you can derive the equation above from a simple linear function.

## Implementation notes

The actual implementation in `src/pow.rs` uses an optimized version to reduce CPU cycles. The gradient
is calculated using precomputed values to avoid unnecessary multiplications and additions. On a
modern x64 processor, this optimisation can reduce each cost calculation from ~17-53 cycles to
~4-8 cycles. Most expensive op is the division which is removed in an optimised Implementation.

To optimize for computation with $x$ as the only dynamic variable, we rearrange the equation:

Starting with:
$$\text{base}(x) = \frac{m - M}{Lq}(x-b) + M$$

Expanding to:
$$\text{base}(x) = \frac{m - M}{Lq} \cdot x - \frac{m - M}{Lq} \cdot b + M$$

Now we can define two precomputable constants:

$$A = \frac{m - M}{Lq}$$

$$B = M - \frac{(m - M) \cdot b}{Lq}$$

This gives us the simplified form:

$$\text{base}(x) = A \cdot x + B$$

$$y = \max(m, \min(M, A \cdot x + B))$$

So the implementation we use precomputes $A$ and $B$ via a config which is passed on every
invocation.
