;;; julia-remoterepl.scm
;;; Steel plugin for Helix: send current selection to a Julia RemoteREPL.jl server.
;;;
;;; Delegates to the bash helper at /home/theo/.config/helix/send-to-remoterepl.sh,
;;; which handles the Julia wire protocol (TCP, serialisation, remotecmd).
;;;
;;; Default keybinding: Alt+Enter  (normal and select modes)
;;; Override with (keymap ...) in your own init.scm after requiring this file.

(require "helix/misc.scm")     ; set-status!, set-warning!, set-error!
(require "helix/static.scm")   ; current-selection->string
(require "helix/ext.scm")      ; hx.with-context, spawn-native-thread
(require "helix/keymaps.scm")  ; keymap macro

(require-builtin steel/process) ; command, with-stdin, spawn-process, wait
(require "steel/result")        ; Ok?, unwrap-ok

(provide send-to-julia-repl)

(define *remoterepl-script* "/home/theo/.config/helix/send-to-remoterepl.sh")
(define *remoterepl-tmpfile* "/tmp/julia_remoterepl_steel.jl")

;;@doc
;; Send the current selection to the running Julia RemoteREPL.jl server.
;;
;; - Reads the selected text with `current-selection->string`.
;; - Writes it to a temp file to avoid any shell-quoting issues.
;; - Feeds the temp file as stdin to `send-to-remoterepl.sh` in a background
;;   thread so the editor stays responsive.
;; - Reports the result (done / error) in the Helix status bar.
(define (send-to-julia-repl)
  (define code (current-selection->string))
  (cond
    [(or (not code) (equal? code ""))
     (set-warning! "julia-remoterepl: nothing selected")]
    [else
     (set-status! "julia-remoterepl: sending…")
     (spawn-native-thread
       (lambda ()
         ;; Write selection to temp file – safer than passing code on the
         ;; command line where quoting / newlines could cause issues.
         (define out (open-output-file *remoterepl-tmpfile*))
         (display code out)
         (close-output-port out)

         ;; Run the bash helper with the temp file as stdin.
         (define stdin-port (open-input-file *remoterepl-tmpfile*))
         (define spawn-result
           (~> (command *remoterepl-script* '())
               (with-stdin stdin-port)
               spawn-process))

         (define exit-code
           (if (Ok? spawn-result)
               (let ([child (unwrap-ok spawn-result)])
                 (define wait-result (wait child))
                 (close-input-port stdin-port)
                 (if (Ok? wait-result) (unwrap-ok wait-result) 1))
               (begin
                 (close-input-port stdin-port)
                 1)))

         ;; Update the status bar on the main Helix thread.
         (hx.with-context
           (lambda ()
             (if (= exit-code 0)
                 (set-status! "julia-remoterepl: done")
                 (set-error! (string-append "julia-remoterepl: error (exit "
                                            (number->string exit-code) ")")))))))]))

;; Register Alt+Enter in normal and select modes.
;; To use a different key, remove these lines and add your own keymap call
;; in init.scm after requiring this file.
(keymap (global)
        (normal (A-ret send-to-julia-repl))
        (select (A-ret send-to-julia-repl)))
