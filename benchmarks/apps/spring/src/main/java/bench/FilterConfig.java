package bench;

import jakarta.servlet.Filter;
import jakarta.servlet.FilterChain;
import jakarta.servlet.ServletException;
import jakarta.servlet.ServletRequest;
import jakarta.servlet.ServletResponse;
import jakarta.servlet.http.HttpServletResponse;

import java.io.IOException;

import org.springframework.boot.web.servlet.FilterRegistrationBean;
import org.springframework.context.annotation.Bean;
import org.springframework.context.annotation.Configuration;

/**
 * The five middlewares the contract requires for {@code /middleware}: real
 * servlet filters, each setting one header and passing the request on.
 *
 * <p>Registered against {@code /middleware} alone rather than {@code /*}, so
 * the other seven endpoints are not measured through a stack the contract does
 * not ask them to carry — the same choice the Rustlavel entry makes with a
 * route group.
 */
@Configuration(proxyBeanMethods = false)
class FilterConfig {

    /** Sets one {@code x-bench-N: ok} header, then calls the next filter. */
    private static final class BenchFilter implements Filter {
        private final String header;

        private BenchFilter(int index) {
            this.header = "x-bench-" + index;
        }

        @Override
        public void doFilter(ServletRequest request, ServletResponse response, FilterChain chain)
                throws IOException, ServletException {
            ((HttpServletResponse) response).setHeader(header, "ok");
            chain.doFilter(request, response);
        }
    }

    private static FilterRegistrationBean<Filter> register(int index) {
        FilterRegistrationBean<Filter> registration = new FilterRegistrationBean<>(new BenchFilter(index));
        registration.addUrlPatterns("/middleware");
        registration.setOrder(index);
        registration.setName("benchFilter" + index);
        return registration;
    }

    @Bean
    FilterRegistrationBean<Filter> benchFilter1() {
        return register(1);
    }

    @Bean
    FilterRegistrationBean<Filter> benchFilter2() {
        return register(2);
    }

    @Bean
    FilterRegistrationBean<Filter> benchFilter3() {
        return register(3);
    }

    @Bean
    FilterRegistrationBean<Filter> benchFilter4() {
        return register(4);
    }

    @Bean
    FilterRegistrationBean<Filter> benchFilter5() {
        return register(5);
    }
}
