import { httpBasic } from "eve/channels/auth";
import { eveChannel } from "eve/channels/eve";

export default eveChannel({
  auth: [httpBasic({ username: "bench", password: process.env.BENCH_TOKEN ?? "bench-token" })],
});
