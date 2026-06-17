import http from "node:http";

const PORT = 8080;

const server = http.createServer((req, res) => {
  const response = {
    origin: req.socket.remoteAddress,
    headers: req.headers,
    url: req.url,
    method: req.method,
    timestamp: new Date().toISOString(),
    message: "Hello from Omnitun test server!"
  };

  res.writeHead(200, {'Content-Type': 'application/json'});
  res.end(JSON.stringify(response, null, 2));
})

server.listen(8080, () => {
  console.log(`Mock server running on http://localhost:${PORT}`);
});