// The relay of a benchmark link (startLink in fixture.mjs). It ends the browser's TCP
// connection on the WAN side and opens a local one to gliss-server, as a reverse
// proxy or Distribox's backend does, so the server never feels the link through its
// own socket. Both sockets send without Nagle's delay, like nginx's and asyncio's.
import {connect, createServer} from 'node:net';

const [host, port, target] = process.argv.slice(2);
createServer(client => {
  const upstream = connect(Number(target), '127.0.0.1');
  for (const [from, to] of [[client, upstream], [upstream, client]]) {
    from.setNoDelay(true);
    from.on('error', () => to.destroy());
    from.pipe(to);
  }
}).listen(Number(port), host, () => process.send('ready'));
