const fs = require('node:fs')

function reverseDiscoveryDisabled(config) {
  let section = ''
  const values = []
  for (const rawLine of config.split(/\r?\n/)) {
    const line = rawLine.trim()
    if (!line || line.startsWith('#') || line.startsWith(';')) continue
    const header = /^\[([^\]]+)\]$/.exec(line)
    if (header) {
      section = header[1]
      continue
    }
    if (section !== 'General') continue
    const setting = /^ReverseServiceDiscovery\s*=\s*(.*)$/.exec(line)
    if (setting) values.push(setting[1])
  }
  return values.length === 1 && values[0] === 'false'
}

if (require.main === module) {
  if (process.argv.length !== 3) {
    process.stderr.write('usage: node bluez-host-policy.cjs /etc/bluetooth/main.conf\n')
    process.exitCode = 2
  } else if (!reverseDiscoveryDisabled(fs.readFileSync(process.argv[2], 'utf8'))) {
    process.stderr.write('dedicated simulator host requires one active [General] ReverseServiceDiscovery = false\n')
    process.exitCode = 1
  } else {
    process.stdout.write('dedicated simulator reverse discovery disabled\n')
  }
}

module.exports = { reverseDiscoveryDisabled }
