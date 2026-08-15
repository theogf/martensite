;;; martensite.scm
;;; Steel plugin for Helix: send current selection to a DaemonicCabal.jl server.
;;;
;;; Requires `temper` to be on PATH.
;;; Default keybinding: C-j (normal and select modes), C-S-j for top-level

(require "helix/misc.scm")        ; set-status!, set-warning!, set-error!, cursor-position, push-component!
(require "helix/static.scm")      ; current-selection->string, get-helix-cwd
(require "helix/editor.scm")      ; set-register!, editor-focus, editor->doc-id, editor->text
(require "helix/ext.scm")         ; hx.with-context, spawn-native-thread
(require "helix/treesitter.scm")  ; document->tree, tstree->root, tsnode-*
(require "helix/components.scm")  ; markdown-component, new-component!, block/render, buffer/clear

(require-builtin steel/process)      ; command, with-stdin, with-stdout-piped, spawn-process, wait->stdout
(require-builtin helix/core/text) ; rope-char->byte, rope->byte-slice, rope->string
(require "steel/result")          ; Ok?, unwrap-ok

(provide send-to-julia-repl)
(provide send-top-level-to-julia-repl)

;; ─── Sending code ────────────────────────────────────────────────────────────

;; temper resolves the session and passes --sync --eval to juliaclient.
(define (run-temper code)
  (define process
    (~> (command "temper" (list code))
        with-stdout-piped
        spawn-process
        unwrap-ok))
  (unwrap-ok (wait->stdout process)))

;; ─── Output popup ───────────────────────────────────────────────────────────
;; There's no single Steel helper for "bordered box, sized to content, anchored
;; near the cursor" — build one the same way the built-in hover-doc popup (and
;; e.g. mattwparas/helix-config's term.scm) does it: a custom component that
;; clears its area and draws a `block` first, then renders a markdown-component
;; inset by the border. row/col/width/height are computed once up front from
;; the cursor position and the content size; render just clamps that box to
;; stay on screen.
(struct OutputPopup (markdown width height row col))

(define (clamp lo hi v)
  (max lo (min hi v)))

(define *popup-max-width* 100)
(define *popup-max-height* 20)

(define (output-popup-render state rect frame)
  (define w (min (OutputPopup-width state) (area-width rect)))
  (define h (min (OutputPopup-height state) (area-height rect)))
  (define max-x (max (area-x rect) (- (+ (area-x rect) (area-width rect)) w)))
  (define max-y (max (area-y rect) (- (+ (area-y rect) (area-height rect)) h)))
  (define x (clamp (area-x rect) max-x (OutputPopup-col state)))
  (define y (clamp (area-y rect) max-y (OutputPopup-row state)))
  (define box (area x y w h))
  (buffer/clear frame box)
  (block/render frame box (make-block (theme-scope-ref "ui.popup") (theme-scope-ref "ui.popup") "all" "rounded"))
  (define inner (area (+ 1 x) (+ 1 y) (- w 2) (- h 2)))
  (render-native-component (OutputPopup-markdown state) inner frame))

;; Dismiss on any keypress, mirroring the built-in doc-popup behavior.
;; handle_event fires for every event (redraws, resizes, etc.), not just key
;; presses, so closing unconditionally closed the popup on the very next
;; non-key tick — check key-event? first.
(define (output-popup-handle-event state event)
  (if (key-event? event) event-result/close event-result/ignore))

;; Show output in a floating, bordered popup sized to fit the output and
;; anchored just below the cursor, or just update the status bar if empty.
(define (show-output! output)
  (define lines (split-many output "\n"))
  (define content-width (apply max 1 (map string-length lines)))
  (define content-height (length lines))
  ;; +4 width: 2 border + 1 space padding each side. +4 height: 2 border + the
  ;; two ``` fence lines wrapping the content.
  (define box-width (clamp 20 *popup-max-width* (+ content-width 4)))
  (define box-height (clamp 3 *popup-max-height* (+ content-height 4)))
  (define cursor (car (current-cursor)))
  (define anchor-row (if cursor (+ 1 (position-row cursor)) 0))
  (define anchor-col (if cursor (position-col cursor) 0))
  (define md (markdown-component (string-append "```\n" output "\n```")))
  (define popup
    (new-component! "martensite-output"
                    (OutputPopup md box-width box-height anchor-row anchor-col)
                    output-popup-render
                    (hash "handle_event" output-popup-handle-event)))
  (push-component! popup))

;; Send code string via temper and display output.
(define (send-code! code)
  (spawn-native-thread
    (lambda ()
      (define output (run-temper code))
      (hx.with-context
        (lambda ()
          (if (equal? output "")
              (set-status! "martensite: done (no output)")
              (show-output! output))))))
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
