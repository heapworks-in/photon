# Call Hierarchy: Flow

A third option beside **Callers** and **Calls** in the Call Hierarchy panel. It shows one graph:
where the flow starts on the left, the traced function in the middle, what it calls on the right.
Mockup: https://claude.ai/artifact/K7R5jk7TwxqittX4XEaBHt

## What it follows

- **Callers** are followed back, level by level, until a branch reaches an entry point, HTTP
  handler or test (the panel's existing tags), or a function nothing calls.
- **Calls** are followed down only through the project's own code. Code outside the project's
  folders (Java's classes, a dependency: the language server's copies open in hidden folders) is
  shown once, with a dashed border, and never expanded. The user asked for exactly this: "lets not
  plot all depths beyond the project, java native + dependencies, just one level".
- A function already on its own path shows once, marked as recursive, as in the list.
- Limits: 6 levels from the traced function, 60 functions in all. A branch stopped by a limit
  shows a "+" and opens on a double-click, like any node.

## Behaviour

- Flow is always the graph view (callers left, calls right); the List/Map buttons hide.
- The graph fills in as the language server answers; the loading line shows its progress.
- Switching back to Callers or Calls traces the same function in that one direction.

## Not covered

- No automatic restart of a language server that stops answering (tracked separately).
- Library detection relies on the language server opening library sources outside the project's
  folders, as the Java extension does.
