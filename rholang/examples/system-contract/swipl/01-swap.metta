(p 1 2)
(p x y)

(= (swap (Pair $x $y)) (Pair $y $x))

; Mandatory entry point with dummy parameter (unused)
(= (main $_)
   (match &self (p $a $b) (swap (Pair $a $b)))
)
