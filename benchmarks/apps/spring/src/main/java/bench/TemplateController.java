package bench;

import java.util.ArrayList;
import java.util.List;

import org.springframework.stereotype.Controller;
import org.springframework.ui.Model;
import org.springframework.web.bind.annotation.GetMapping;

/** 8. Fifty rows through Thymeleaf, the framework's own server-side template engine. */
@Controller
class TemplateController {

    @GetMapping("/template")
    String template(Model model) {
        List<Dtos.TableRow> rows = new ArrayList<>(50);
        for (int id = 1; id <= 50; id++) {
            rows.add(new Dtos.TableRow(id, "User " + id));
        }
        model.addAttribute("title", "Benchmark");
        model.addAttribute("rows", rows);
        return "table";
    }
}
