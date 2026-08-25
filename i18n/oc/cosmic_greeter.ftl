shutdown = Atudar
restart-timeout =
    Lo sistèma reaviarà automaticament
    { $seconds ->
        [0] ara.
        [1] d’aquí 1 segondas.
       *[other] d’aquí { $seconds } segondas.
    }
shutdown-timeout =
    Lo sistèma s'atudarà automaticament
    { $seconds ->
        [0] ara.
        [1] d’aquí 1 segondas.
       *[other] d’aquí { $seconds } segondas.
    }
