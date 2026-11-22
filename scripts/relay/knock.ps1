# Knock on the relay VM (scripts/relay/knockd.py, docs/RELAY.md): SSH opens to this address for 60 s.
# The knock file is one line "host port key-hex", readable only by you (never in this repository).
# usage: powershell -File knock.ps1 <knock file>        then ssh / PuTTY / WinSCP within 60 s
param([Parameter(Mandatory = $true)][string]$KnockFile)
$ErrorActionPreference = 'Stop'
$f = (Get-Content -Raw $KnockFile).Trim() -split '\s+'
$target = $f[0]; $port = [int]$f[1]
$key = [byte[]]::new($f[2].Length / 2)
for ($i = 0; $i -lt $key.Length; $i++) { $key[$i] = [Convert]::ToByte($f[2].Substring(2 * $i, 2), 16) }
$ts = [BitConverter]::GetBytes([UInt64][DateTimeOffset]::UtcNow.ToUnixTimeSeconds())
[Array]::Reverse($ts)  # big endian
$nonce = [byte[]]::new(16)
[Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($nonce)
$label = [Text.Encoding]::ASCII.GetBytes('gangli-knock-v1')
$hm = [Security.Cryptography.HMACSHA256]::new($key)
$mac = $hm.ComputeHash([byte[]]($label + $ts + $nonce))
$pkt = [byte[]]($ts + $nonce + $mac)
$u = [Net.Sockets.UdpClient]::new()
[void]$u.Send($pkt, $pkt.Length, $target, $port)
$u.Close()
Write-Output "knocked on $target"
