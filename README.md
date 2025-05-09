# Alpen Labs faucet API

## how to claim

First, call `GET /pow_challenge/<chain>`, where `<chain>` is `l1` or `l2`, which will return something like this:

```json
{
  "nonce": "<16 byte hex string>",
  "difficulty": <0 to 255>
}
```

This will only fail if you call it over IPv6, where it will respond with a `422 Unprocessable Entity` code, or when the faucet has insufficient funds, where it will respond with a `503 Service Unavailable`.

As the client, you are challenged to then find a solution where:

```rs
let salt = b"strata faucet 2024";
// nonce is the 16 decoded bytes from the API
// solution is a 8 byte array
// `|` is representing concatenation
return count_leading_zeros(sha256(salt | nonce | solution)) >= difficulty;

fn count_leading_zeros(data: &[u8]) -> u8 {
    let mut leading_zeros = 0;
    for byte in data {
        if *byte == 0 {
            leading_zeros += 8;
        } else {
            leading_zeros += byte.leading_zeros() as u8;
            break;
        }
    }

    leading_zeros
}
```

For those who are compiler challenged, there are 0-dependency JavaScript and Python implementations of solvers in the `utils` directory. You can easily run the JS solver in your browser by running `just html-solver` and then navigating to http://localhost:3001.

Once you find a solution, hex encode it and use it in a claim for either L1 or L2 funds:

### L1

`GET /claim_l1/<solution_as_hex>/<l1_address>`

Where `l1_address` is the address that you want to receive funds on.

If successful, this will return a `200 OK` with the hex-encoded txid in the body.
If not, it will return a status code and a raw error message string in the body.

### L2

`GET /claim_l2/<solution_as_hex>/<l2_address>`

Where `l2_address` is the address that you want to receive funds on.

If successful, this will return a `200 OK` with the hex-encoded txid in the body.
If not, it will return a status code and a raw error message string in the body.

## License

This work is dual-licensed under MIT and Apache 2.0.
You can choose between one of them if you use this work.
