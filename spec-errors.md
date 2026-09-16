# AGWPE API documentation errors

The AGWPE TCP/IP API tutorial is at
<https://www.on7lds.net/42/sites/default/files/AGWPEAPI.HTM>.  It is a
useful interoperability reference, but it is not a complete normative
specification.  This document records contradictions and errors found in
it.

## Incorrect DataKind byte values

* The table for `V` (Send UNPROTO VIA) labels it `ASCII 0x76`.
  `0x76` is lowercase `v`, the Connect VIA command; uppercase `V` is
  `0x56`.
* The application-side table for `g` (Ask Port Capabilities) labels it
  `ASCII 0x6D`.  `0x6D` is `m`; `g` is `0x67`.  The document's
  response-side `g` table correctly gives `0x67`.

Use literal command bytes, such as `b'V'` and `b'g'`, rather than
copying these hexadecimal values.

## Broken C++ send example

The C++ `SendPacket` example writes a 36-byte header and then calls
`TXDATA(szTemp, count+26)`.  It must send the 36-byte header plus exactly
`count` data bytes.  As written, it can send a truncated header or fewer
data bytes than the declared `DataLen`.  It also copies `count+1` data bytes
while declaring `DataLen = count`, leaving an extra byte in the stream if it
were otherwise sent in full.

Do not use this example as a wire-format implementation.

## Ambiguous port-field description

The common header table says that the AGWPE port field is one byte, but also
describes a least-significant and a most-significant byte.  Elsewhere the
document explains the actual convention: API port zero denotes the first
port shown by the AGWPE UI, API port one denotes the second, and so on.  The
three following header bytes are reserved and should be zero.

## Underspecified behavior

* The document does not make a single normative statement that all
  multi-byte integers are little-endian.  Its examples and individual field
  layouts use little-endian encoding.
* It provides no correlation identifier, reply-ordering guarantees, or
  full connection state machine.  Applications must tolerate unsolicited
  frames interleaved with replies and correlate connection events by their
  route.
* The `v` Connect VIA payload only specifies NUL-terminated digipeater
  callsigns.  It does not specify a representation for AX.25's
  has-been-repeated (H) bit.  A `CALL*` digipeater is therefore a
  Direwolf-compatible extension, not an AGWPE behavior defined by this
  document.  Fresh outgoing connection requests normally have no seen
  hops.
