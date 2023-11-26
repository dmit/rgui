# rgui

A basic terminal UI for searching text files using regular expressions.

```console
$ rgui [-p <pattern>] [<search_paths...>]
```

```
Positional Arguments:
  search_paths      list of paths to search

Options:
  -p, --pattern     pattern to search for
  --help            display usage information
```

## TODO

- [ ] Toggle for case insensitive search.
- [ ] Toggle for plain text (non-regex) search.
- [ ] Progress marker for when a search is ongoing.
- [ ] Basic edit controls on the pattern field (move cursor left/right, select all, etc).
- [ ] Navigation of results and, possibly, ability to launch `$EDITOR` on files at the line where the currently selected match was found.
- [ ] Restore previous console state after quitting instead of clearing the screen.
- [x] A `--help` flag?
- [ ] Show an error in case of a malformed search pattern.

## License

[The Unlicense](LICENSE).
