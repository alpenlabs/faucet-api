async function sha256(data) {
  const buffer = await crypto.subtle.digest('SHA-256', data);
  return new Uint8Array(buffer);
}

function countLeadingZeros(data) {
  let leadingZeros = 0;
  for (let byte of data) {
    if (byte === 0) {
      leadingZeros += 8;
    } else {
      leadingZeros += byte.toString(2).padStart(8, '0').indexOf('1');
      break;
    }
  }
  return leadingZeros;
}

async function findSolution(nonce, difficulty) {
  const salt = new Uint8Array([
    0x73, 0x74, 0x72, 0x61, 0x74, 0x61, 0x20, 0x66,
    0x61, 0x75, 0x63, 0x65, 0x74, 0x20, 0x32, 0x30,
    0x32, 0x34
  ]);

  nonce = new Uint8Array(nonce.match(/.{1,2}/g).map(byte => parseInt(byte, 16)));
  let solution = new Uint8Array(8);

  while (true) {
    const hashInput = new Uint8Array([...salt, ...nonce, ...solution]);
    const hash = await sha256(hashInput);
    if (countLeadingZeros(hash) >= difficulty) {
      return Array.from(solution).map(byte => byte.toString(16).padStart(2, '0')).join('');
    }
    // Increment solution
    for (let i = 7; i >= 0; i--) {
      if (solution[i] < 0xFF) {
        solution[i]++;
        break;
      } else {
        solution[i] = 0;
      }
    }
  }
}

onmessage = async function (event) {
  const { nonce, difficulty } = event.data;
  const solution = await findSolution(nonce, difficulty);
  postMessage({ solution });
};
