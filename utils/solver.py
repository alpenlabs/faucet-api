import hashlib

# SHA-256 hashing function
def sha256(data):
    return hashlib.sha256(data).digest()

# Count leading zeros in a byte array
def count_leading_zeros(data):
    leading_zeros = 0
    for byte in data:
        if byte == 0:
            leading_zeros += 8
        else:
            leading_zeros += bin(byte).find('1')
            break
    return leading_zeros

# Find solution
def find_solution(nonce, difficulty):
    salt = bytes.fromhex("737472617461206661756365742032303234")
    nonce = bytes.fromhex(nonce)
    solution = bytearray(8)

    while True:
        hash_input = salt + nonce + solution
        hash = sha256(hash_input)
        print(hash.hex())
        print(count_leading_zeros(hash))
        if count_leading_zeros(hash) >= difficulty:
            return solution.hex()
        # Increment solution
        for i in range(7, -1, -1):
            if solution[i] < 0xFF:
                solution[i] += 1
                break
            else:
                solution[i] = 0

# Example usage
nonce = "4bbbefa849c59704f7f13745ca47161a"  # Replace with actual nonce
difficulty = 17  # Replace with actual difficulty
solution = find_solution(nonce, difficulty)
print("Solution:", solution)
