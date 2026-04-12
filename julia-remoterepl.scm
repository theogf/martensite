;;; julia-remoterepl.scm
;;; Steel plugin for Helix: send current selection to a DaemonicCabal.jl server.
;;;
;;; Requires `juliaclient` to be on PATH.
;;; Default keybinding: Alt+Enter  (normal and select modes)

(require "helix/misc.scm")     ; set-status!, set-warning!, set-error!
(require "helix/static.scm")   ; current-selection->string
(require "helix/editor.scm")   ; set-register!
(require "helix/ext.scm")      ; hx.with-context, spawn-native-thread
(require "helix/keymaps.scm")  ; keymap macro

(require-builtin steel/process) ; command, with-stdin, spawn-process, wait
(require "steel/result")        ; Ok?, unwrap-ok

(provide send-to-julia-repl)

(define *remoterepl-tmpfile* "/tmp/julia_remoterepl_steel.jl")

;; ─── Sending code ────────────────────────────────────────────────────────────

;; Pipe tmpfile as stdin to juliaclient.
;; Returns the process exit code (integer).
(define (run-juliaclient session tmpfile)
  (define stdin-port (open-input-file tmpfile))
  (define result
    (~> (command "juliaclient" (list "--session" session))
        (with-stdin stdin-port)
        spawn-process
        unwrap-ok
        wait))
  (close-input-port stdin-port)
  (unwrap-ok result))

;; ─── Main command ────────────────────────────────────────────────────────────

;;@doc
;; Send the current selection to the running DaemonicCabal.jl server.
;; On failure, copies a server startup command to the clipboard.
(define (send-to-julia-repl)
  (define code (current-selection->string))
  (cond
    [(or (not code) (equal? code ""))
     (set-warning! "julia-remoterepl: nothing selected")]
    [else
     (set-status! "julia-remoterepl: sending…")
     (spawn-native-thread
       (lambda ()
         (define out (open-output-file *remoterepl-tmpfile*))
         (display code out)
         (close-output-port out)
         (define exit-code (run-juliaclient (get-helix-cwd) *remoterepl-tmpfile*))
         (delete-file! *remoterepl-tmpfile*)
         (hx.with-context
           (lambda ()
             (if (= exit-code 0)
                 (set-status! "julia-remoterepl: done")
                 (begin
                   (set-register! #\+ (list "using DaemonicCabal; DaemonicCabal.serve()"))
                   (set-error! "julia-remoterepl: juliaclient failed — startup cmd copied to clipboard")))))))]))

;; Register Alt+Enter in normal and select modes.
(keymap (global)
        (normal (A-ret send-to-julia-repl))
        (select (A-ret send-to-julia-repl)))
