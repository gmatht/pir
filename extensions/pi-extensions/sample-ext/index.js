/*
 * Sample legacy pi extension.
 *
 * This demonstrates the pi extension surface that the Node.js shim supports:
 *   - pi.on(event, handler):        register a lifecycle event handler
 *   - pi.registerTool({...}):       expose a tool the agent can call
 *   - pi.registerCommand(name, {}): expose a REPL slash-command
 *
 * The `requires` array (also in package.json) lists the ABI features this
 * extension needs. The PIR `pi-extensions` extension compares that against the
 * host ABI and warns (and can offer an agent-assisted fix) for any gap.
 *
 * Intentionally, this extension declares it needs `ctx.ui.input` (a TUI widget
 * feature the shim does NOT support) so the compatibility-warning path in PIR
 * has something to flag. Remove that entry from `requires` to see a clean load.
 */
'use strict';

module.exports = {
  requires: ['events.session_start', 'pi.registerTool', 'ctx.ui.input'],

  register(pi) {
    pi.on('session_start', () => {
      pi.ctx.ui.notify('sample-ext: hello from a legacy pi extension', 'info');
    });

    pi.registerTool({
      name: 'greet',
      description: 'Return a friendly greeting from the sample extension.',
      schema: {
        type: 'object',
        properties: {
          name: { type: 'string', description: 'who to greet' },
        },
        required: ['name'],
      },
      execute(args) {
        const who = (args && args.name) || 'world';
        return `hello, ${who}! (from the node shim)`;
      },
    });

    pi.registerTool({
      name: 'add',
      description: 'Add two numbers and return the sum.',
      schema: {
        type: 'object',
        properties: {
          a: { type: 'number' },
          b: { type: 'number' },
        },
        required: ['a', 'b'],
      },
      execute(args) {
        const a = Number((args && args.a) || 0);
        const b = Number((args && args.b) || 0);
        return { sum: a + b };
      },
    });

    pi.registerCommand('sample-ping', {
      description: 'Sample extension slash command: replies pong.',
      handler(arg) {
        return `pong${arg ? ' ' + arg : ''}`;
      },
    });
  },
};
