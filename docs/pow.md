# Proof-of-work in the faucet

The faucet uses POW challenges for anti-dos and dynamically shifts the
difficulty based on the faucet's balance.

Generally, the less bitcoin in the faucet, the more difficult it is to get out
via solving the POW challenge.

This was initially implemented via a logarithmic based function but since the
difficulty parameter already exponentially increases the difficulty, we switched
to a linear approach which is easier to read and is less likely to blow up.

Output: **$y$**, difficulty parameter. Difficulty is $2^y$. Amounts are in sats.

Parameters:
- **$x$**: Current balance of the faucet. Positive integer.
- **$m$**: Minimum difficulty parameter, a good default is about 17-20. Between 0 and **$M$**.
- **$q$**: Amount in sats emitted per successful faucet request. Positive integer.
- **$L$**: Difficulty linear increase coeffcient. A good default is 10-25. Positive integer.
- **$b$**: Minimum balance. It will be computationally impossible to drop the faucet's balance below this value. Defaults to 0. Positive integer.

Constants:
- **$M$**: Maximum difficulty parameter = 255

$$\text{base}(x) = \frac{m - M}{Lq}(x-b) + M$$

$$y = \max(m, \min(M, \text{base}(x)))$$

Derivation method:

For $x \leq b$, $y = M$. I.e, when the faucet is at or below the minimum balance, the difficulty is maximum.

At $x \geq b + Lq$, $y = m$. When the faucet has at least $b + Lq$, the difficulty is minimum.

$L$ controls the rate at which the difficulty increases. A higher $L$ means it will increase slower, but the difficulty will start increasing earlier.

For example, if $L = 10$, $b = 0$ and $q = 10000$, we'll start ramping up the difficulty once the faucet's balance drops below $b + Lq = 100000$ sats.

Given these rules, you can derive the equation above from a simple linear function.
