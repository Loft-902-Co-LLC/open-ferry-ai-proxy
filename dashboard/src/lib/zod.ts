// zod compiles fast object parsers with `new Function` when it may. The
// dashboard's Content-Security-Policy allows no eval, and zod's probe for it
// would be reported as a violation, so its plain parsers are used instead.
// Every module imports z from here (the lint enforces it), so this runs
// before any schema parses.
import { z } from "zod";

z.config({ jitless: true });

export { z };
