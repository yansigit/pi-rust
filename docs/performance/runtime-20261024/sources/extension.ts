// Synthetic fixture: one tool, no imports, hooks, network, timers or init I/O.
export default function (pi) {
  pi.registerTool({
    name: "bench_echo_INDEX",
    label: "Benchmark echo INDEX",
    description: "Return the supplied text unchanged.",
    parameters: { type: "object", properties: { text: { type: "string" } }, required: ["text"] },
    async execute(_id, params) {
      return { content: [{ type: "text", text: params.text }], details: {} };
    }
  });
}
