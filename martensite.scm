;;; martensite.scm
;;; Steel plugin for Helix: send current selection to a DaemonicCabal.jl server.
;;;
;;; Requires `temper` to be on PATH.
;;; Default keybinding: C-j (normal and select modes), C-S-j for top-level

(require "helix/misc.scm")        ; set-status!, set-warning!, set-error!, cursor-position, push-component!, pop-last-component-by-name!
(require "helix/static.scm")      ; current-selection->string, get-helix-cwd
(require "helix/editor.scm")      ; set-register!, editor-focus, editor->doc-id, editor->text
(require "helix/ext.scm")         ; hx.with-context, spawn-native-thread
(require "helix/treesitter.scm")  ; document->tree, tstree->root, tsnode-*
(require "helix/components.scm")  ; new-component!, block/render, buffer/clear, Color/*, style*, frame-set-string!

(require-builtin steel/process)      ; command, with-stdout-piped, with-stderr-piped, spawn-process, child-stdout, child-stderr, wait
(require-builtin steel/ports)        ; read-port-to-string
(require-builtin helix/core/text) ; rope-char->byte, rope->byte-slice, rope->string
(require "steel/result")          ; Ok?, unwrap-ok

;; VTE (terminal emulator) support for rendering ANSI-colored output in the
;; popup — already installed for this Helix config's own term.scm. Renders
;; captured stdout/stderr through a real ANSI parser instead of stripping
;; colors, so Julia's colored errors/values show up as they would in a
;; terminal.
(#%require-dylib "libsteel_pty"
                 (only-in raw-virtual-terminal
                          vte/resize
                          vte/advance-bytes
                          vte/reset-iterator!
                          vte/advance-iterator!
                          vte/iter-x
                          vte/iter-y
                          vte/iter-cell-fg
                          vte/iter-cell-bg
                          vte/iter-cell-str
                          term/color-attribute))

(provide send-to-julia-repl)
(provide send-top-level-to-julia-repl)

;; ─── Sending code ────────────────────────────────────────────────────────────

;; temper resolves the session and passes --sync --eval to juliaclient.
;; Both stdout and stderr are piped and captured — Julia writes errors and
;; stacktraces to stderr, not stdout, and an unpiped stderr is inherited from
;; Helix's own process: it was writing raw text straight to the terminal,
;; bypassing the TUI compositor (and this plugin's popup) entirely.
;;
;; wait->stdout can't be combined with child-stderr: internally it takes the
;; whole child handle (via Rust's Child::wait_with_output, which reads stdout
;; AND stderr but only hands back stdout), so by the time it returns there is
;; no child left to pull stderr from — child-stderr then errors on #false.
;; Grabbing both port handles before waiting, and waiting separately after,
;; avoids that.
(define (run-temper code)
  (define process
    (~> (command "temper" (list code))
        with-stdout-piped
        with-stderr-piped
        spawn-process
        unwrap-ok))
  (define out (read-port-to-string (child-stdout process)))
  (define err (read-port-to-string (child-stderr process)))
  (wait process)
  (string-append out err))

;; ─── Output popup ───────────────────────────────────────────────────────────
;; A custom component (new-component!) that draws a bordered block, then
;; renders the captured output through a real VTE (steel-pty's
;; raw-virtual-terminal) instead of plain fenced markdown text — Julia colors
;; its errors/values with ANSI SGR codes, and feeding those bytes straight to
;; the VTE lets it interpret them (and do its own line-wrapping) rather than
;; needing them stripped or hand-truncated first.
(struct OutputPopup (vte width height row col))

(define (clamp lo hi v)
  (max lo (min hi v)))

;; Kept small: the original 100x20 cap was routinely larger than the actual
;; terminal, which is what made the box look like it was "everywhere" — it
;; wasn't a rendering bug, just an oversized box with nothing there to clip it
;; further than the (already large) editor area.
(define *popup-max-width* 60)
(define *popup-max-height* 10)

;; The VTE's actual row count — much taller than the popup ever shows, so a
;; long stacktrace doesn't scroll its own beginning off the top before
;; render time. See show-output!.
(define *popup-vte-scrollback-rows* 300)

;; term/color-attribute converts an opaque TermColorAttribute into (list r g b
;; a), an indexed-palette int, or #false for "use the theme default". There's
;; no functional "build me an indexed Color" constructor — mirrors steel-pty's
;; own term.scm, which mutates a scratch Color in place via
;; set-color-indexed!/set-color-rgb! instead.
(define (attr->color attr)
  (cond
    [(list? attr)
     (define c (Color/rgb (car attr) (cadr attr) (caddr attr)))
     c]
    [(int? attr)
     (define c (Color/rgb 0 0 0))
     (set-color-indexed! c attr)
     c]
    [else #f]))

;; vte/iter-cell-fg and vte/iter-cell-bg return a raw #false — not a
;; TermColorAttribute — whenever the iterator has no "last cell" set yet;
;; term/color-attribute only accepts the opaque TermColorAttribute type, so
;; it must never be called on that #false.
(define (cell-color-attr raw)
  (if raw (term/color-attribute raw) #f))

;; Builds a fresh Style per cell rather than reusing/defaulting one: there's
;; no way to pull a bare Color? back out of theme->fg/theme->bg (they return
;; a whole Style?, which is what set-style-fg!/set-style-bg! choked on when
;; used as a fallback color). Leaving fg/bg unset on a Default-attribute cell
;; instead means "don't touch what's already there" — which is exactly the
;; block's theme colors that block/render already painted underneath.
(define (cell-style vte)
  (define s (style))
  (define fg (attr->color (cell-color-attr (vte/iter-cell-fg vte))))
  (define bg (attr->color (cell-color-attr (vte/iter-cell-bg vte))))
  (when fg (set-style-fg! s fg))
  (when bg (set-style-bg! s bg))
  s)

(define (output-popup-render state rect frame)
  (define w (min (OutputPopup-width state) (area-width rect)))
  (define h (min (OutputPopup-height state) (area-height rect)))
  (define max-x (max (area-x rect) (- (+ (area-x rect) (area-width rect)) w)))
  (define max-y (max (area-y rect) (- (+ (area-y rect) (area-height rect)) h)))
  (define x (clamp (area-x rect) max-x (OutputPopup-col state)))
  (define y (clamp (area-y rect) max-y (OutputPopup-row state)))
  (define box (area x y w h))
  (buffer/clear frame box)
  (block/render frame box (make-block (theme->bg *helix.cx*) (theme->fg *helix.cx*) "all" "rounded"))
  ;; +2, not +1: 1 cell for the border plus 1 cell of padding, so content
  ;; doesn't render flush against the border wall.
  (define inner-x (+ 2 x))
  (define inner-y (+ 2 y))
  ;; Window into the VTE's tall scrollback (see *popup-vte-scrollback-rows*
  ;; / show-output!): only the rows that actually fit the padded interior.
  (define visible-rows (- h 4))
  (define vte (OutputPopup-vte state))
  (vte/reset-iterator! vte)
  (let loop ()
    (when (vte/advance-iterator! vte)
      ;; vte/iter-cell-str, like vte/iter-cell-fg/-bg, returns raw #false
      ;; (not a string) whenever the iterator has no "last cell" set yet —
      ;; skip those cells rather than handing #false to frame-set-string!.
      (define cell-str (vte/iter-cell-str vte))
      (when (and (string? cell-str) (< (vte/iter-y vte) visible-rows))
        (frame-set-string! frame
                           ;; vte/iter-x is 1-indexed (first column reports
                           ;; as 1, not 0) — verified against steel-pty
                           ;; directly; vte/iter-y is 0-indexed, no
                           ;; adjustment needed there.
                           (+ inner-x (- (vte/iter-x vte) 1))
                           (+ inner-y (vte/iter-y vte))
                           cell-str
                           (cell-style vte)))
      (loop))))

;; Dismiss on any keypress, mirroring the built-in doc-popup behavior.
;; handle_event fires for every event (redraws, resizes, etc.), not just key
;; presses, so closing unconditionally closed the popup on the very next
;; non-key tick — check key-event? first.
(define (output-popup-handle-event state event)
  (if (key-event? event) event-result/close event-result/ignore))

;; Show output in a floating, bordered popup fixed at *popup-max-width* x
;; *popup-max-height* and anchored just below the cursor, or just update the
;; status bar if empty. The box size is fixed rather than content-derived
;; (unlike the old markdown-based popup): the VTE does its own line-wrapping
;; internally, so pre-computing a tight content size for e.g. a long
;; stacktrace line isn't straightforward — this is a deliberate simplification
;; over the old truncate-to-fit behavior.
(define (show-output! output)
  ;; Replace any popup from a still-open previous call rather than stacking a
  ;; new one on top of it (e.g. two send-to-julia-repl calls with no dismiss
  ;; keypress in between).
  (pop-last-component-by-name! "martensite-output")
  (define box-width *popup-max-width*)
  (define box-height *popup-max-height*)
  (define vte (raw-virtual-terminal))
  ;; Rows: far taller than the popup ever shows (*popup-vte-scrollback-rows*),
  ;; windowed down to the visible rows in output-popup-render. A long Julia
  ;; stacktrace is many more lines than the popup's visible height, and a VTE
  ;; sized to exactly the visible rows scrolls like a real terminal as it's
  ;; fed — showing only the *tail* of the trace (deep internal frames) and
  ;; cutting off the actual "ERROR: ..." message and the user's own frames at
  ;; the top, which is the useful part.
  ;;
  ;; Cols: -4, not -2: border (2) + padding (2), matching
  ;; output-popup-render's inner-x/inner-y inset — otherwise the VTE wraps
  ;; content wider than the padded interior actually has room for.
  (vte/resize vte *popup-vte-scrollback-rows* (- box-width 4))
  ;; The captured output is piped (not a real pty), so it's plain Unix text
  ;; with bare \n — no kernel tty driver is present to translate that to
  ;; \r\n. This VTE is a faithful raw terminal emulator, so \n alone is just
  ;; a linefeed (moves down a row) and does NOT return the cursor to column
  ;; 0 the way a real terminal's ICRNL/ONLCR settings would; without this,
  ;; every line after the first starts wherever the previous line's cursor
  ;; ended up, staircasing further right each line. Verified directly
  ;; against steel-pty outside of Helix before landing this fix.
  (vte/advance-bytes vte (string-replace output "\n" "\r\n"))
  (define cursor (car (current-cursor)))
  (define anchor-row (if cursor (+ 1 (position-row cursor)) 0))
  (define anchor-col (if cursor (position-col cursor) 0))
  (define popup
    (new-component! "martensite-output"
                    (OutputPopup vte box-width box-height anchor-row anchor-col)
                    output-popup-render
                    (hash "handle_event" output-popup-handle-event)))
  (push-component! popup))

;; Runs on the background thread spawned by send-code! — split out so
;; with-handler below wraps a single call, not a multi-expression body whose
;; interaction with internal defines under with-handler isn't something I've
;; verified.
(define (send-code-inner! code)
  (define output (run-temper code))
  (hx.with-context
    (lambda ()
      (if (equal? output "")
          (set-status! "martensite: done (no output)")
          (show-output! output)))))

;; Send code string via temper and display output. Errors on the background
;; thread (e.g. in run-temper or show-output!) otherwise vanish silently —
;; spawn-native-thread has no visible failure path — so this surfaces
;; whatever goes wrong via set-error! instead of guessing blind.
(define (send-code! code)
  (spawn-native-thread
    (lambda ()
      (with-handler
        (lambda (e)
          (define msg (call-with-output-string (lambda (port) (display e port))))
          (hx.with-context (lambda () (set-error! (string-append "martensite error: " msg)))))
        (send-code-inner! code))))
  (set-status! "martensite: sending…"))

;; ─── Main commands ───────────────────────────────────────────────────────────

;;@doc
;; Send the current selection to the running DaemonicCabal.jl server.
;; On failure, copies a server startup command to the clipboard.
(define (send-to-julia-repl)
  (define code (string-join (register->value #\.) "\n"))
  (cond
    [(or (not code) (equal? code ""))
     (set-warning! "martensite: nothing selected")]
    [else
     (send-code! code)]))

;; Walk up the tree-sitter tree until we reach a direct child of the root.
(define (find-top-level-node node)
  (define parent (tsnode-parent node))
  (cond
    [(not parent) node]
    [(not (tsnode-parent parent)) node]
    [else (find-top-level-node parent)]))

;;@doc
;; Send the top-level tree-sitter form under the cursor to the running
;; DaemonicCabal.jl server. On failure, copies a startup command to the clipboard.
(define (send-top-level-to-julia-repl)
  (define doc-id (editor->doc-id (editor-focus)))
  (define tree (document->tree doc-id))
  (cond
    [(not tree)
     (set-warning! "martensite: no tree-sitter tree for this buffer")]
    [else
     (define rope (editor->text doc-id))
     (define cursor-char (cursor-position))
     (define cursor-byte (rope-char->byte rope cursor-char))
     (define root (tstree->root tree))
     (define node-at-cursor
       (tsnode-named-descendant-byte-range root cursor-byte cursor-byte))
     (cond
       [(not node-at-cursor)
        (set-warning! "martensite: no node at cursor")]
       [else
        (define top-node (find-top-level-node node-at-cursor))
        (define start-byte (tsnode-start-byte top-node))
        (define end-byte (tsnode-end-byte top-node))
        (define code (rope->string (rope->byte-slice rope start-byte end-byte)))
        (cond
          [(or (not code) (equal? code ""))
           (set-warning! "martensite: top-level node is empty")]
          [else
           (send-code! code)])])]))
