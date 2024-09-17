# Alpen Labs faucet API

## how to claim

First, call `GET /get_pow_challenge` which will return something like this:

```json
{
  "nonce": "<16 byte hex string>",
  "difficulty": <0 to 255>
}
```

This will only fail if you call it over IPv6, where it will respond with a `503 SERVICE UNAVAILABLE` code.

As the client, you are challenged to then find a solution where:

```rs
// b"alpen labs faucet 2024"
let salt = 0x616c70656e206c616273206661756365742032303234;
// nonce is the 16 decoded bytes from the API
// solution is a 8 byte array
// `|` is representing concatenation
count_leading_zeros(sha256(salt | nonce | solution)) >= difficulty
```

Once you find a solution, hex encode it and use it in a claim for either L1 or L2 funds:

### L1

`GET /claim_l1/<solution_as_hex>/<l1_address>`

Where `l1_address` is the address that you want to receive funds on.

If successful, this will return a `200 OK` with an empty body.
If not, it will return a status code and a raw error message string in the body.

### L2

`todo`
